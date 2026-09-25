use path_clean::PathClean;
use sparse_cp::SparseCopyStats;
use sparse_cp::sparse_copy_with_progress;
use std::fs;
use std::mem::take;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use crate::archive::ArchiveRTArgs;
use crate::common::at_least_one_running;
use crate::common::files::warn_if_times_changed;
use crate::db::Database;
use crate::db::ErrorPhase;
use crate::db::Recorder;
use crate::db::flags::ErrorFlags;
use crate::db::sparsify::SparseOutcome;
use crate::db::types::{FilePhase, StrippedRecord};
use crate::error::{Error, Result};
use crate::progress::BarKind;
use crate::shutdown::Shutdown;
use crossbeam_channel::{Receiver, Sender, bounded};
use indicatif::ProgressBar;

// TODO via args
const BATCH_SIZE: u64 = 10_000;

// Producer/consumer pipeline bounds: the input queue takes over the old
// whole-batch pull's memory guard, but workers stream file-by-file so a big
// file no longer stalls every other row's commit and progress.
const WORK_CAPACITY: usize = BATCH_SIZE as usize;
const OUT_CAPACITY: usize = 2 * WORK_CAPACITY;
const FEED_CHUNK: usize = 1_024;      // rows pulled from the DB cursor per round
const DRAIN_CHUNK: usize = BATCH_SIZE as usize / 2;  // outcomes committed per transaction

const ERROR_PHASE: ErrorPhase = ErrorPhase::Pipeline(crate::config::PipelinePhase::Sparsify);

/// Deletes `path` on drop unless [`keep`](Self::keep) was called.
struct TempSparseFile {
    path: PathBuf,
    keep: bool,
}

impl TempSparseFile {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    /// Mark path to be kept.
    fn keep(mut self) {
        self.keep = true;
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempSparseFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Sparsify stage: optional sparse rewrites under `stage/sp.{content_id}`.
pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    debug_assert_ne!(rt.config.sparse.page_size, 0, "Expected page_size > 0");
    if rt.config.sparse.page_size == 0 {
        return Err(Error::Config("page_size must be greater than 0".into()));
    }

    tracing::info!(
        page_size =rt. config.sparse.page_size,
        min_pages = ?rt.config.sparse.min_pages,
        "sparsify pass"
    );

    let stage_dir = rt.config.paths.stage_dir();
    fs::create_dir_all(&stage_dir).map_err(|e| Error::io(&stage_dir, e))?;

    // Skip sparsify operation.
    if !rt.config.sparse.sparsify {
        let n = rt.db.promote_deduped_to_sparsified()?;
        rt.progress.inc_global(n);
        rt.db.drop_sparsify_queue()?;
        tracing::info!(count = n, "promoted all deduped → sparsified (min_pages unset)");
        return Ok(());
    };

    // PRECONDITION: min_page set.
    let skipped = rt.db.promote_non_sparsify_candidates_to_sparsified(rt.config.sparse.min_pages)?;
    rt.progress.inc_global(skipped);
    tracing::info!(count = skipped, "promoted non-candidates → sparsified");

    // Ordering table: encodes size-DESC order (position column) so the feed
    // can pull pending rows in that order without a long-lived SQL cursor.
    // Populate is idempotent (INSERT OR IGNORE); dropped only on success.
    rt.db.create_sparsify_queue()?;
    rt.db.populate_sparsify_queue(rt.config.sparse.min_pages)?;

    // The phase bar tracks the whole workload across sessions: `count_all` is
    // phase-agnostic, `pending` the session "todo", and their difference the
    // already-done position — so a resumed run still shows prior progress.
    let workload = rt.db.count_all_sparsify_candidates(rt.config.sparse.min_pages)?;
    let pending = rt.db.count_pending_sparsify_candidates(rt.config.sparse.min_pages)?;
    rt.progress.set_phase_total(workload);
    rt.progress.set_phase_position(workload.saturating_sub(pending));
    if pending == 0 {
        rt.db.drop_sparsify_queue()?;
        sanity_no_deduped(rt.db)?;
        return Ok(());
    }

    // One bar per worker, materialized now so each worker can take its bar by
    // value; workers never touch `ProgressBarSet`.
    let jobs = rt.config.process.io_jobs;
    rt.progress.create_thread_bars(BarKind::Bytes, jobs);
    let mut bars = Vec::<ProgressBar>::new();
    for i in 0..jobs {
        bars.push(rt.progress.thread_bar(i));
    }

    let (work_s, work_r) = bounded::<StrippedRecord>(WORK_CAPACITY);
    let (out_s, out_r) = bounded::<Option<SparseOutcome>>(OUT_CAPACITY);
    let mut thread_handles = Vec::with_capacity(jobs);

    for i in 0..jobs {
        let bar = bars[i].clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let sh = rt.shutdown.clone();
        let sd = stage_dir.clone();
        let ps = rt.config.sparse.page_size;
        let res = thread::Builder::new()
            .name(format!("sparsify-worker-{i}").into())
            .spawn(move || sparsify_worker(bar, sd, ps, sh, wr, os))
            .expect("spawn sparsify worker");
        thread_handles.push(res);
    }
    drop(work_r);
    drop(out_s);

    let (completed, errored) = run_enqueue_dequeue_loop_sparsify(
        &rt, work_s, out_r, thread_handles
    )?;
    rt.progress.drop_thread_bars();

    match rt.shutdown.is_interrupted() {
        true => {
            let msg = if rt.shutdown.is_force() {
                "sparsify force-aborted; in-flight progress discarded"
            } else {
                "sparsify stopped; completed files saved"
            };
            tracing::warn!(saved = completed, "{msg}");
            Err(Error::Interrupted)
        }
        false => {
            rt.db.drop_sparsify_queue()?;
            sanity_no_deduped(rt.db)?;
            tracing::info!(ok = completed - errored, err = errored, "sparsify complete");
            Ok(())
        }
    }
}

/// Perform the full enqueue / dequeue loop in batches to improve performance.
/// The state machine for the enqueue / dequeue process is quite involved and
/// pollutes the name space of the function, which is why it is moved to a
/// separate function. Owns the worker handles: they are joined before the
/// final drain so a trailing `None` can never race `drop(recv)`.
fn run_enqueue_dequeue_loop_sparsify(
    rt: &ArchiveRTArgs,
    send: Sender<StrippedRecord>,
    recv: Receiver<Option<SparseOutcome>>,
    mut handles: Vec<thread::JoinHandle<()>>,
) -> Result<(u64, u64)> {
    let mut recorder = Recorder::new(rt.db, !rt.config.process.no_errors);

    // Feed cursor over `sparsify_queue`: `queue_index` is the last consumed
    // queue position. The pull filters to still-pending rows, so a file already
    // handed to a worker (or sparsified on a previous run) is never re-pulled.
    let mut queue_index = 0u64;
    let mut feed_idx = 0usize;
    let mut feed_exhausted = false;
    let mut feed_buf = Vec::<(u64, StrippedRecord)>::new();
    let mut busy = false;

    let mut dequeue_total = 0u64;
    let mut exited_workers = 0u64;
    let mut feed_total = 0u64;
    let mut completed = 0u64;
    let mut errored = 0u64;
    let mut pending_out = Vec::<SparseOutcome>::new();

    let mut apply_chunk = |pending: &mut Vec<SparseOutcome>| -> Result<(u64, u64)> {
        let items = take(pending);
        if items.is_empty() {
            return Ok((0, 0));
        }
        let n = items.len() as u64;
        rt.db.ingest_sparsify_outcome(&items)?;
        let mut err = 0u64;
        for res in items {
            match &res.err {
                None => (),
                Some(e) => match e {
                    Error::Interrupted => (),
                    Error::FileStat(_) => {
                        record_sparsify_error(&mut recorder, &res);
                        err += 1;
                    }
                    other => panic!(
                        "Invariant Error. Only FileStatError and Interrupted expected. Got: {other}"
                    ),
                },
            }
        }
        rt.progress.inc_both(n);
        Ok((n, err))
    };

    let mut drain_chunk = |
        is_busy: &mut bool, drain_override: bool,
        dequeue: &mut u64, exit: &mut u64|
        -> Result<()> {
        // Drain finished outcomes into a small batch.
        while pending_out.len() < DRAIN_CHUNK {
            match recv.try_recv() {
                Ok(Some(res)) => {
                    pending_out.push(res);
                    *is_busy = true;
                    *dequeue += 1;
                }
                Ok(None) => *exit += 1,
                Err(_) => break,
            }
        }
        if pending_out.len() >= DRAIN_CHUNK || drain_override {
            let (n, e) = apply_chunk(&mut pending_out)?;
            completed += n;
            errored += e;
            *is_busy = true;
        }
        Ok(())
    };

    loop {
        // Any pending shutdown (graceful *or* force) stops the feed: workers
        // observe it between files / in the copy callback and either finish or
        // abort, so keeping the feed open would only pile up rows nobody
        // consumes (and on force could wedge the loop in a full-channel retry).
        if rt.shutdown.is_interrupted() { break; }
        if feed_idx == feed_buf.len() {
            feed_buf = rt.db.pull_pending_sparsify_rows::<StrippedRecord>(
                queue_index, FEED_CHUNK as u64)?;
            feed_idx = 0;
            if feed_buf.is_empty() {
                feed_exhausted = true;
            } else {
                queue_index = feed_buf[feed_buf.len() - 1].0;
            }
        }
        while feed_idx < feed_buf.len() {
            match send.try_send(feed_buf[feed_idx].1.clone()) {
                Ok(_) => { feed_total += 1; busy = true; feed_idx += 1 }
                Err(_) => break,
            }
        }

        drain_chunk(&mut busy, false, &mut dequeue_total, &mut exited_workers)?;
        // Leave for dequeue loop.
        if feed_exhausted && feed_idx == feed_buf.len()
            || !at_least_one_running(&handles.iter().collect()) {
            break;
        }
        if !busy {
            thread::sleep(Duration::from_millis(10));
        }
        busy = false;
    }

    // Cut the feed side; idle workers end their receive loop.
    drop(send);

    // Drain to completion. A row handed to a worker becomes durable only once
    // its outcome is applied here, so the channel must stay open until every
    // fed row is accounted for — dropping it earlier turns the last `out.send`
    // into a Disconnected panic and loses the outcome. Steady state ends the
    // instant `feed_total` outcomes are pulled. An interrupt may drop rows (a
    // worker stopped between files produces no outcome for it), so that case
    // falls back to a short quiet window after the last received outcome —
    // long enough for the in-flight file to finish or be aborted, whichever
    // comes first.
    loop {
        if rt.shutdown.is_interrupted()
            || dequeue_total == feed_total
            || !at_least_one_running(&handles.iter().collect())
            || exited_workers == rt.config.process.io_jobs as u64 {
            break;
        }
        drain_chunk(&mut busy, false, &mut dequeue_total, &mut exited_workers)?;
        if !busy {
            thread::sleep(Duration::from_millis(4));
        }
        busy = false;
    }

    // Join every worker. Each one exits right after `send` closed, so their
    // trailing `None` is already in the channel (or lands before the final
    // drain below) — joining first makes `drop(recv)` race-free.
    for handle in take(&mut handles) {
        let _ = handle.join();
    }
    drain_chunk(&mut busy, true, &mut dequeue_total, &mut exited_workers)?;
    drop(recv);
    recorder.flush()?;
    Ok((completed, errored))
}

/// Record a per-file sparsify failure in the persistent error log (best-effort).
/// Only called when `err` carries `Error::FileStat(_)`.
fn record_sparsify_error(recorder: &mut Recorder, o: &SparseOutcome) {
    recorder.record_file(
        o.id,
        ERROR_PHASE,
        o.err.as_ref().expect("record_sparsify_error: err must be Some").to_file_stat(None),
        ErrorFlags::default(),
    );
}

/// Worker thread: pulls one file at a time, sparsifies it into the stage dir,
/// forwards the outcome. Owns its bar; only touches the channels and `tracing`.
fn sparsify_worker(
    bar: ProgressBar,
    stage_dir: PathBuf,
    page_size: usize,
    shutdown: Shutdown,
    work: Receiver<StrippedRecord>,
    out: Sender<Option<SparseOutcome>>) -> () {
    loop {
        // INFO: Destroy channel to exit the worker!
        match work.recv() {
            Ok(row) => {
                if shutdown.check_between_files().is_err() {
                    break;
                }
                bar.reset();
                bar.set_length(row.size);
                bar.set_message(format!("Sparsifying {}", row.abs_path.display()));
                let modified = warn_if_times_changed(
                    &row.abs_path, row.mtime, row.atime, row.ctime,
                );
                let name = row
                    .sparse_member_name()
                    .expect("Invariant: sparsify candidates must be self-canonical files");
                let dst = stage_dir.join(name).clean();
                let tmp = TempSparseFile::new(dst);
                let outcome = match sparse_one(
                    &row.abs_path, tmp.path(), page_size, &shutdown, Some(&bar)) {
                    Ok(_) => {
                        tmp.keep();
                        SparseOutcome { id: row.id, modified, err: None }
                    }
                    Err(err) => SparseOutcome { id: row.id, modified, err: Some(err) },
                };
                out.send(Some(outcome)).expect("sparsify worker: result channel closed");
            }
            Err(_) => break,
        }
    }
    out.send(None).expect("sparsify worker: result channel closed");
}

/// Copy `path` → `dst` sparsely, reporting byte progress on `pb`. A force
/// interrupt aborts inside the copy loop (graceful lets the in-flight file
/// finish); the partial `dst` is cleaned up by the caller's `TempSparseFile`.
fn sparse_one(
    path: &Path,
    dst: &Path,
    page_size: usize,
    shutdown: &Shutdown,
    pb: Option<&ProgressBar>)
    -> Result<SparseCopyStats> {
    let mut last = 0u64;
    sparse_copy_with_progress(path, dst, page_size, |pos, _size, _dur| {
        if let Some(pb) = pb {
            let delta = pos.saturating_sub(last);
            if delta > 0 {
                pb.inc(delta);
            }
            last = pos;
        }
        if shutdown.is_force() {
            Err(Error::Interrupted)
        } else {
            Ok(())
        }
    })
}

/// Sanity check that we have no files left in the dedup phase.
fn sanity_no_deduped(db: &Database) -> Result<()> {
    let leftover = db.count_files_in_phase(FilePhase::Deduped)?;
    if leftover != 0 {
        panic!(
            "INVARIANT ERROR: \
             sparsify finished with {leftover} file(s) still in deduped (expected 0)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::DedupMode;
    use crate::common::files::original_extension;
    use crate::common::start::StartPolicy;
    use crate::config::{
        ArchiveConfig, ArchivePipelineOptions, CaptureOptions, CleanupSettings, CompressionFormat,
        CompressionSettings, FilterOptions, IndexingOptions, InputOptions, OwnerPolicy, PathLayout,
        ProcessOptions, SparseOptions,
    };
    use crate::db::flags::FileFlag;
    use crate::db::types::{FileId, FileType, NewFileRecord};
    use crate::error::FileStatError;
    use crate::progress::{ARCHIVE_MULTIPLIER, ProgressBarSet};
    use chrono::{DateTime, Utc};
    use nix::unistd::geteuid;
    use rusqlite::named_params;
    use std::os::unix::fs::PermissionsExt;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::PathBuf;

    fn zeros(n: usize) -> Vec<u8> {
        vec![0u8; n]
    }

    fn test_archive_config() -> ArchiveConfig {
        ArchiveConfig {
            paths: PathLayout {
                archive_path: PathBuf::new(),
                directory: PathBuf::new(),
                work_dir: PathBuf::new(),
            },
            inputs: InputOptions {
                input_dirs: Vec::new(),
                files_from: Vec::new(),
                files_from_null: false,
            },
            indexing: IndexingOptions {
                no_recursion: false,
                dereference: false,
                one_file_system: false,
                no_hardlink_detection: true,
                no_strict_separation: false,
            },
            filter: FilterOptions {
                exclude_patterns: Vec::new(),
                include_patterns: Vec::new(),
                exclude_from: Vec::new(),
                include_from: Vec::new(),
                anchored: false,
                ignore_case: false,
                eager_filter: false,
            },
            capture: CaptureOptions {
                do_xattrs: false,
                do_posix_acl: false,
                do_selinux: false,
                numeric_ids_only: false,
                mode: None,
                transform: None,
            },
            owner_policy: OwnerPolicy {
                owner: None, owner_map: None, group: None, group_map: None,
            },
            sparse: SparseOptions { sparsify: true, page_size: 4096, min_pages: 4 },
            compression: CompressionSettings {
                format: CompressionFormat::None, level: 0, xz_extreme: false,
                memlimit_compress: None,
            },
            process: ProcessOptions {
                start_policy: StartPolicy::Create,
                jobs: 4,
                io_jobs: 1,
                fail_fast: false,
                no_errors: false,
                cleanup: CleanupSettings { keep_db: false, keep_stage: false },
                exit_after_stage: None,
            },
            pipeline: ArchivePipelineOptions {
                dedup_mode: DedupMode::Regular,
                write_archive_footer: false,
                clear_archive_meta: false,
            },
        }
    }

    struct TestWorld {
        dir: tempfile::TempDir,
        db: Database,
        shutdown: Shutdown,
        progress: ProgressBarSet,
        config: ArchiveConfig,
    }

    impl TestWorld {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let db = Database::open(&dir.path().join("test.sqlite")).expect("open db");
            db.with_transaction(|conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO filter_reason_archive (id, source, line, expression) \
                     VALUES (-1, 'internal', NULL, '.*')", [],
                ).expect("seed internal include rule");
                Ok(())
            }).expect("seed internal include rule tx");
            let mut config = test_archive_config();
            config.paths.work_dir = dir.path().join("astage");
            // `run()` creates the stage dir itself, but worker/loop-level tests
            // drive the loop directly and still target it — make it up front.
            fs::create_dir_all(&config.paths.work_dir).expect("create stage dir");
            Self {
                dir, db,
                shutdown: Shutdown::detached(),
                progress: ProgressBarSet::new(ARCHIVE_MULTIPLIER),
                config,
            }
        }

        fn rt(&self) -> ArchiveRTArgs<'_> {
            ArchiveRTArgs {
                config: &self.config,
                db: &self.db,
                shutdown: &self.shutdown,
                progress: &self.progress,
            }
        }

        fn rt_with<'a>(&'a self, shutdown: &'a Shutdown) -> ArchiveRTArgs<'a> {
            ArchiveRTArgs {
                config: &self.config,
                db: &self.db,
                shutdown,
                progress: &self.progress,
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn add_file(&self, name: &str, payload: &[u8]) -> FileId {
            let path = self.dir.path().join(name);
            std::fs::write(&path, payload).expect("write test payload");
            self.db.insert_file(&NewFileRecord {
                abs_path: PathBuf::from(&path),
                ext: original_extension(&path),
                size: payload.len() as u64,
                mtime: None, atime: None, ctime: None,
                uid: None, gid: None, mode: None,
                ftype: Some(FileType::File),
                xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None, link_dst: None,
                device_id: None, inode_id: None, major: None, minor: None,
            }).expect("insert test file");
            self.db.file_id_by_abs_path(&path).expect("lookup test file").expect("file present")
        }

        fn add_file_recorded(
            &self, name: &str, payload: &[u8],
            mtime: Option<DateTime<Utc>>, atime: Option<DateTime<Utc>>, ctime: Option<DateTime<Utc>>)
            -> FileId {
            let path = self.dir.path().join(name);
            std::fs::write(&path, payload).expect("write test payload");
            self.db.insert_file(&NewFileRecord {
                abs_path: PathBuf::from(&path),
                ext: original_extension(&path),
                size: payload.len() as u64,
                mtime, atime, ctime,
                uid: None, gid: None, mode: None,
                ftype: Some(FileType::File),
                xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None, link_dst: None,
                device_id: None, inode_id: None, major: None, minor: None,
            }).expect("insert recorded file");
            self.db.file_id_by_abs_path(&path).expect("lookup test file").expect("file present")
        }

        /// Apply the include-all filter, then promote the row to a deduped
        /// self-canonical candidate with the given `sparse_count`. Sets the
        /// filter columns directly so repeated calls don't disturb siblings.
        fn seed_dedup_row(&self, id: FileId, sparse_count: u64) {
            self.db.with_transaction(|conn| {
                let n = conn.execute(
                    "UPDATE files SET phase = 'deduped', canonical_id = :id, \
                     sparse_count = :n, sha1 = :sha1, \
                     include_reason_archive = -1, exclude_reason_archive = 0 \
                     WHERE id = :id",
                    named_params! {
                        ":id": id.0,
                        ":n": sparse_count as i64,
                        ":sha1": [7u8; 20].as_slice(),
                    },
                ).expect("seed dedup row");
                assert_eq!(n, 1);
                Ok(())
            }).expect("seed dedup row tx");
        }

        /// The `run()` preamble minus the per-worker spawn: promotions +
        /// ordering queue. Lets loop-level tests reuse `run`'s DB state.
        fn prepare_for_sparsify(&self) {
            self.db.promote_non_sparsify_candidates_to_sparsified(
                self.config.sparse.min_pages,
            ).expect("promote non-candidates");
            self.db.create_sparsify_queue().expect("create queue");
            self.db.populate_sparsify_queue(self.config.sparse.min_pages).expect("populate");
        }

        fn phase(&self, id: FileId) -> FilePhase {
            self.db.get_file_by_id::<StrippedRecord>(id)
                .expect("get row")
                .expect("row present")
                .phase
        }

        fn flag(&self, id: FileId, flag: FileFlag) -> bool {
            self.db.get_file_flag(id, flag).expect("get flag")
        }

        /// The canonical row's candidate so its `sp.{content_id}` stage file
        /// target can be checked on disk.
        fn record(&self, id: FileId) -> StrippedRecord {
            self.db.get_file_by_id::<StrippedRecord>(id)
                .expect("get row")
                .expect("row present")
        }

        fn stage_path(&self, id: FileId) -> Option<PathBuf> {
            let name = self.record(id).sparse_member_name();
            name.map(|n| self.config.paths.work_dir.clone().join(n))
        }
    }

    /// One worker + one bar, driving `run_enqueue_dequeue_loop_sparsify`
    /// directly. Returns `(completed, errored)`.
    fn run_loop(world: &TestWorld, bar: ProgressBar, work_cap: usize) -> (u64, u64) {
        let (work_s, work_r) = bounded::<StrippedRecord>(work_cap);
        let (out_s, out_r) = bounded::<Option<SparseOutcome>>(OUT_CAPACITY);
        let sh = world.shutdown.clone();
        let sd = world.config.paths.work_dir.clone();
        let ps = world.config.sparse.page_size;
        let wr = work_r.clone();
        let os = out_s.clone();
        let mut handles = Vec::<thread::JoinHandle<()>>::new();
        let worker = thread::Builder::new().name("sparsify-worker-test".into())
            .spawn(move || sparsify_worker(bar, sd, ps, sh, wr, os))
            .expect("spawn sparsify worker");
        handles.push(worker);
        drop(work_r);
        drop(out_s);
        let rt = world.rt();
        run_enqueue_dequeue_loop_sparsify(&rt, work_s, out_r, handles)
            .expect("sparsify loop")
    }

    #[test]
    fn sparse_one_writes_sparse_destination() {
        let world = TestWorld::new();
        let src = world.path("src.bin");
        let dst = world.path("dst.bin");
        let payload = zeros(4 * 4096);
        std::fs::write(&src, &payload).expect("write src");

        let stats = sparse_one(&src, &dst, 4096, &Shutdown::detached(), None)
            .expect("sparse_one ok");

        assert_eq!(stats.size_in, payload.len() as u64);
        assert_eq!(stats.zero_blocks, 4);
        assert_eq!(stats.bytes_saved, 4 * 4096);
        let meta = std::fs::metadata(&dst).expect("dst metadata");
        assert_eq!(meta.len(), payload.len() as u64);
    }

    #[test]
    fn sparse_one_inaccessible_returns_filestat() {
        if geteuid().is_root() {
            return;
        }
        let world = TestWorld::new();
        let src = world.path("src.bin");
        let dst = world.path("dst.bin");
        std::fs::write(&src, &zeros(64 * 1024)).expect("write src");
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0)).expect("chmod 000");

        let res = sparse_one(&src, &dst, 4096, &Shutdown::detached(), None);
        assert!(matches!(res, Err(Error::FileStat(_))));
    }

    #[test]
    fn sparse_one_force_interrupts_mid_copy() {
        let world = TestWorld::new();
        let src = world.path("src.bin");
        let dst = world.path("dst.bin");
        std::fs::write(&src, &zeros(4 * 4096)).expect("write src");
        let force = Shutdown::detached();
        force.request_force();

        let res = sparse_one(&src, &dst, 4096, &force, None);
        assert!(matches!(res, Err(Error::Interrupted)));
    }

    #[test]
    fn sparsify_worker_sends_none_on_channel_close() {
        let world = TestWorld::new();
        let (work_s, work_r) = bounded::<StrippedRecord>(2);
        let (out_s, out_r) = bounded::<Option<SparseOutcome>>(8);

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh = world.shutdown.clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let sd = world.config.paths.work_dir.clone();
        let worker = thread::Builder::new().name("sparsify-worker-test".into())
            .spawn(move || sparsify_worker(bar, sd, 4096, sh, wr, os))
            .expect("spawn sparsify worker");
        drop(work_r);
        drop(out_s);
        drop(work_s);   // close the work channel: worker recv -> Err -> None

        assert!(matches!(out_r.recv().expect("recv terminal"), None));
        let _ = worker.join();
        world.progress.drop_thread_bars();
    }

    #[test]
    fn sparsify_worker_sends_none_on_graceful_preset() {
        let world = TestWorld::new();
        let id = world.add_file("a.bin", &zeros(4 * 4096));
        world.seed_dedup_row(id, 4);
        let row = world.record(id);
        let (work_s, work_r) = bounded::<StrippedRecord>(2);
        let (out_s, out_r) = bounded::<Option<SparseOutcome>>(8);
        work_s.send(row).expect("send row");
        world.shutdown.request_graceful();
        drop(work_s);

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh = world.shutdown.clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let sd = world.config.paths.work_dir.clone();
        let worker = thread::Builder::new().name("sparsify-worker-test".into())
            .spawn(move || sparsify_worker(bar, sd, 4096, sh, wr, os))
            .expect("spawn sparsify worker");
        drop(work_r);
        drop(out_s);

        // The row is pulled but graceful is pre-set: no outcome, just None.
        assert!(matches!(out_r.recv().expect("recv terminal"), None));
        let _ = worker.join();
        world.progress.drop_thread_bars();
    }

    #[test]
    fn sparsify_worker_success_outcome_and_stage_file() {
        let world = TestWorld::new();
        let id = world.add_file("a.bin", &zeros(4 * 4096));
        world.seed_dedup_row(id, 4);
        let row = world.record(id);
        let (work_s, work_r) = bounded::<StrippedRecord>(2);
        let (out_s, out_r) = bounded::<Option<SparseOutcome>>(8);

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh = world.shutdown.clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let sd = world.config.paths.work_dir.clone();
        let worker = thread::Builder::new().name("sparsify-worker-test".into())
            .spawn(move || sparsify_worker(bar, sd, 4096, sh, wr, os))
            .expect("spawn sparsify worker");
        drop(work_r);
        drop(out_s);
        work_s.send(row).expect("send row");
        drop(work_s);

        match out_r.recv().expect("recv outcome") {
            Some(o) => {
                assert_eq!(o.id, id);
                assert_eq!(o.modified, false);
                assert!(matches!(o.err, None));
            }
            None => panic!("expected an outcome, got None"),
        }
        assert!(matches!(out_r.recv().expect("recv terminal"), None));
        let _ = worker.join();
        world.progress.drop_thread_bars();

        assert!(std::fs::metadata(&world.stage_path(id).expect("stage path"))
            .is_ok(), "sparse rewrite must exist in the stage dir");
    }

    #[test]
    fn sparsify_worker_panics_when_result_channel_closed() {
        let world = TestWorld::new();
        let id = world.add_file("a.bin", &zeros(4 * 4096));
        world.seed_dedup_row(id, 4);
        let row = world.record(id);
        let (work_s, work_r) = bounded::<StrippedRecord>(2);
        let (out_s, out_r) = bounded::<Option<SparseOutcome>>(8);
        work_s.send(row).expect("send row");

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh = world.shutdown.clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let sd = world.config.paths.work_dir.clone();
        let worker = thread::Builder::new().name("sparsify-worker-test".into())
            .spawn(move || sparsify_worker(bar, sd, 4096, sh, wr, os))
            .expect("spawn sparsify worker");
        drop(work_r);
        drop(work_s);
        // Drop the only receiver: the worker's `out.send(...).expect(...)`
        // must panic on the disconnected channel rather than swallow it.
        drop(out_r);

        let res = worker.join();
        assert!(res.is_err(), "sparsify_worker must panic on a closed result channel");
        world.progress.drop_thread_bars();
    }

    #[test]
    fn loop_exit_via_dequeued_eq_feed_total() {
        let world = TestWorld::new();
        let sizes = [8 * 1024 * 1024, 4 * 1024 * 1024 + 1, 1024 * 1024];
        let mut ids = Vec::<FileId>::new();
        for (i, size) in sizes.iter().enumerate() {
            let id = world.add_file(format!("f{i}.bin").as_str(), &zeros(*size));
            world.seed_dedup_row(id, 4);
            ids.push(id);
        }
        world.prepare_for_sparsify();

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let (completed, errored) = run_loop(&world, world.progress.thread_bar(0), 2);
        world.progress.drop_thread_bars();

        assert_eq!(completed, 3);
        assert_eq!(errored, 0);
        for id in ids {
            assert_eq!(world.phase(id), FilePhase::Sparsified);
            assert!(world.flag(id, FileFlag::HasSparse));
        }
    }

    #[test]
    fn loop_exit_via_graceful_preset_stops_feed() {
        let world = TestWorld::new();
        let id = world.add_file("a.bin", &zeros(1024 * 1024));
        world.seed_dedup_row(id, 4);
        world.prepare_for_sparsify();
        world.shutdown.request_graceful();

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let (completed, errored) = run_loop(&world, world.progress.thread_bar(0), 2);
        world.progress.drop_thread_bars();

        // Graceful pre-set: the feed breaks immediately, nothing is sparsified.
        assert_eq!(completed, 0);
        assert_eq!(errored, 0);
        assert_eq!(world.phase(id), FilePhase::Deduped);
    }

    #[test]
    fn loop_applies_partial_batch_no_loss() {
        let world = TestWorld::new();
        let mut ids = Vec::<FileId>::new();
        for i in 0..3 {
            let id = world.add_file(format!("f{i}.bin").as_str(), &zeros(1024 * 1024));
            world.seed_dedup_row(id, 4);
            ids.push(id);
        }
        world.prepare_for_sparsify();

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let (completed, errored) = run_loop(&world, world.progress.thread_bar(0), 2);
        world.progress.drop_thread_bars();

        // work_cap=2 with 3 rows forces the final override drain to cover the
        // ragged tail — no outcome is lost.
        assert_eq!(completed, 3);
        assert_eq!(errored, 0);
        for id in ids {
            assert!(world.flag(id, FileFlag::HasSparse));
        }
    }

    #[test]
    fn loop_panics_on_other_error_variant() {
        let world = TestWorld::new();
        let (work_s, _work_r) = bounded::<StrippedRecord>(2);
        let (out_s, out_r) = bounded::<Option<SparseOutcome>>(8);
        // A worker never produces this, but the loop's drain must panic anyway:
        // only FileStat / Interrupted are valid outcome errors.
        out_s.send(Some(SparseOutcome {
            id: FileId(1),
            modified: false,
            err: Some(Error::Config("boom".into())),
        })).expect("push bad outcome");
        drop(out_s);
        let handles = Vec::<thread::JoinHandle<()>>::new();
        // The feed needs the ordering queue to exist (pull joins it); an empty
        // world supplies the early feed-exhausted path into the final drain.
        world.prepare_for_sparsify();

        let res = catch_unwind(AssertUnwindSafe(|| -> Result<(u64, u64)> {
            let rt = world.rt();
            run_enqueue_dequeue_loop_sparsify(&rt, work_s, out_r, handles)
        }));
        assert!(res.is_err(), "an invalid error variant must panic the drain");
    }

    #[test]
    fn loop_panicked_worker_gets_no_special_treatment() {
        let world = TestWorld::new();
        for i in 0..3 {
            let id = world.add_file(format!("f{i}.bin").as_str(), &zeros(1024 * 1024));
            world.seed_dedup_row(id, 4);
        }
        world.prepare_for_sparsify();

        let (work_s, work_r) = bounded::<StrippedRecord>(2);
        let (out_s, out_r) = bounded::<Option<SparseOutcome>>(8);
        // A worker that dies before ever touching the channels: no outcome and
        // no trailing None arrives — the loop must still terminate cleanly via
        // its liveness guard and swallow the panicked join.
        let worker = thread::Builder::new().name("sparsify-worker-test".into())
            .spawn(move || panic!("worker crashed"))
            .expect("spawn panicking worker");
        let mut handles = Vec::<thread::JoinHandle<()>>::new();
        handles.push(worker);
        drop(work_r);
        drop(out_s);

        let rt = world.rt();
        let done = run_enqueue_dequeue_loop_sparsify(&rt, work_s, out_r, handles);

        assert!(matches!(done, Ok((0, 0))));
        for id in [FileId(1), FileId(2), FileId(3)] {
            assert_eq!(world.phase(id), FilePhase::Deduped);
        }
    }

    #[test]
    fn run_disabled_promotes_all_no_flags() {
        let mut world = TestWorld::new();
        let id = world.add_file("a.bin", &zeros(1024 * 1024));
        world.seed_dedup_row(id, 4);
        world.config.sparse.sparsify = false;

        run(&world.rt()).expect("run disabled");

        assert_eq!(world.phase(id), FilePhase::Sparsified);
        assert_eq!(world.flag(id, FileFlag::HasSparse), false);
        assert_eq!(
            world.db.count_pending_sparsify_candidates(world.config.sparse.min_pages)
                .expect("no pending"),
            0
        );
    }

    #[test]
    fn run_errors_when_stage_uncreatable() {
        let world = TestWorld::new();
        let id = world.add_file("a.bin", &zeros(1024 * 1024));
        world.seed_dedup_row(id, 4);
        // Replace the (auto-created) stage dir with a regular file so
        // `create_dir_all` in run() fails.
        std::fs::remove_dir_all(&world.config.paths.work_dir).expect("remove stage dir");
        std::fs::write(&world.config.paths.work_dir, &[0u8; 1][..]).expect("write blocker");

        let res = run(&world.rt());
        assert!(matches!(res, Err(Error::FileStat(_))));
        if let Err(Error::FileStat(fse)) = res {
            assert_eq!(fse.io_path(), Some(world.config.paths.work_dir.clone()));
        }
        assert_eq!(world.phase(id), FilePhase::Deduped);
    }

    #[test]
    fn run_sparsifies_candidates_and_flags() {
        let world = TestWorld::new();
        let mut ids = Vec::<FileId>::new();
        for i in 0..3 {
            let id = world.add_file(format!("f{i}.bin").as_str(), &zeros(1024 * 1024));
            world.seed_dedup_row(id, [4, 8, 1][i]);
            ids.push(id);
        }
        // A dup-canonical row: not a candidate, promoted without HasSparse.
        let dup = world.add_file("dup.bin", &zeros(1024 * 1024));
        world.db.with_transaction(|conn| {
            conn.execute(
                "UPDATE files SET phase = 'deduped', canonical_id = :canon, \
                 sparse_count = 4, sha1 = :sha1, \
                 include_reason_archive = -1, exclude_reason_archive = 0 \
                 WHERE id = :id",
                named_params! {
                    ":canon": ids[0].0,
                    ":id": dup.0,
                    ":sha1": [7u8; 20].as_slice(),
                },
            ).expect("seed dup row");
            Ok(())
        }).expect("seed dup tx");

        run(&world.rt()).expect("run");

        for (i, id) in ids.iter().enumerate() {
            let expect_sparse = i < 2;   // sparse_count 4/8 are candidates; 1 is not
            assert_eq!(world.phase(*id), FilePhase::Sparsified);
            assert_eq!(world.flag(*id, FileFlag::HasSparse), expect_sparse);
            if expect_sparse {
                let stage = world.stage_path(*id).expect("stage path");
                assert!(std::fs::metadata(&stage).is_ok(), "stage file must exist");
            }
        }
        assert_eq!(world.phase(dup), FilePhase::Sparsified);
        assert_eq!(world.flag(dup, FileFlag::HasSparse), false);
        // queue is dropped and no deduped rows remain on success.
        assert_eq!(world.db.count_files_in_phase(FilePhase::Deduped).expect("no deduped"), 0);
    }

    #[test]
    fn run_graceful_and_force_preset_return_interrupted() {
        for mode in [0, 1] {
            let world = TestWorld::new();
            let id = world.add_file("a.bin", &zeros(1024 * 1024));
            world.seed_dedup_row(id, 4);
            if mode == 0 {
                world.shutdown.request_graceful();
            } else {
                world.shutdown.request_force();
            }

            let res = run(&world.rt());
            assert!(matches!(res, Err(Error::Interrupted)));
            // Nothing was applied, and the queue survives the interrupt.
            assert_eq!(world.phase(id), FilePhase::Deduped);
            assert_eq!(world.flag(id, FileFlag::HasSparse), false);
            assert_ne!(
                world.db.count_pending_sparsify_candidates(world.config.sparse.min_pages)
                    .expect("count pending"),
                0
            );
        }
    }

    #[test]
    fn run_graceful_interrupt_mid_run_resume_completes() {
        let world = TestWorld::new();
        let mut ids = Vec::<FileId>::new();
        // Non-zero payloads force real writes on the copy so the trigger has a
        // measurable window; sparse_count is a stored column, so seeding it is
        // all the candidate predicate needs.
        for i in 0..4 {
            let mut payload = Vec::<u8>::new();
            payload.resize_with(16 * 1024 * 1024, || i as u8);
            let id = world.add_file(format!("f{i}.bin").as_str(), &payload);
            world.seed_dedup_row(id, 4);
            ids.push(id);
        }
        world.prepare_for_sparsify();

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh2 = world.shutdown.clone();
        let bar_obs = bar.clone();
        let trigger = thread::spawn(move || {
            // Fire while the first copy is in flight: graceful lets that file
            // finish, then the worker stops between files.
            for _ in 0..20_000 {
                if bar_obs.position() > 0 {
                    sh2.request_graceful();
                    return;
                }
                thread::sleep(Duration::from_millis(1));
            }
            sh2.request_graceful();
        });

        let (completed, _errored) = run_loop(&world, bar, 2);
        trigger.join().expect("join trigger");
        world.progress.drop_thread_bars();

        assert!(completed >= 1);
        assert!(completed < ids.len() as u64);
        let pending = world.db
            .count_pending_sparsify_candidates(world.config.sparse.min_pages).expect("pending");
        assert_eq!(pending, ids.len() as u64 - completed);
        // queue survives the interrupt for the resume.
        assert_ne!(
            world.db.pull_pending_sparsify_rows::<StrippedRecord>(0, 100)
                .expect("queue survives interrupt").len(),
            0
        );

        // Resume with a fresh shutdown through run(): everything completes.
        let sh3 = Shutdown::detached();
        run(&world.rt_with(&sh3)).expect("resume run");
        for id in ids {
            assert_eq!(world.phase(id), FilePhase::Sparsified);
            assert!(world.flag(id, FileFlag::HasSparse));
        }
        assert_eq!(world.db.count_pending_sparsify_candidates(
            world.config.sparse.min_pages).expect("no pending"), 0);
    }

    #[test]
    fn run_force_interrupt_mid_run_discards_in_flight() {
        let world = TestWorld::new();
        let mut ids = Vec::<FileId>::new();
        for i in 0..4 {
            let mut payload = Vec::<u8>::new();
            payload.resize_with(8 * 1024 * 1024, || i as u8);
            let id = world.add_file(format!("f{i}.bin").as_str(), &payload);
            world.seed_dedup_row(id, 4);
            ids.push(id);
        }
        world.prepare_for_sparsify();

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh2 = world.shutdown.clone();
        let bar_obs = bar.clone();
        let trigger = thread::spawn(move || {
            for _ in 0..20_000 {
                if bar_obs.position() > 0 {
                    sh2.request_force();
                    return;
                }
                thread::sleep(Duration::from_millis(1));
            }
            sh2.request_force();
        });

        let (completed, errored) = run_loop(&world, bar, 2);
        trigger.join().expect("join trigger");
        world.progress.drop_thread_bars();

        // Force aborts the in-flight copy: it yields Interrupted, never an error
        // record, and the row stays deduped for the resume.
        assert_eq!(errored, 0);
        assert!(completed < ids.len() as u64);
        assert!(world.db.count_pending_sparsify_candidates(
            world.config.sparse.min_pages).expect("pending") >= 1);

        let sh3 = Shutdown::detached();
        run(&world.rt_with(&sh3)).expect("resume run");
        for id in ids {
            assert_eq!(world.phase(id), FilePhase::Sparsified);
            assert!(world.flag(id, FileFlag::HasSparse));
        }
    }

    #[test]
    fn unreadable_file_recorded_not_fatal() {
        if geteuid().is_root() {
            return;
        }
        let world = TestWorld::new();
        let payload = zeros(4 * 4096);
        let id_ok = world.add_file("ok.bin", &payload);
        let id_bad = world.add_file("bad.bin", &payload);
        let id_ok2 = world.add_file("ok2.bin", &payload);
        world.seed_dedup_row(id_ok, 4);
        world.seed_dedup_row(id_bad, 4);
        world.seed_dedup_row(id_ok2, 4);
        std::fs::set_permissions(&world.path("bad.bin"), std::fs::Permissions::from_mode(0))
            .expect("chmod 000");

        run(&world.rt()).expect("run tolerates an unreadable candidate");

        assert_eq!(world.phase(id_bad), FilePhase::Sparsified);
        assert!(world.flag(id_bad, FileFlag::ErrorWhileSparsify));
        assert_eq!(world.flag(id_bad, FileFlag::HasSparse), false);
        for id in [id_ok, id_ok2] {
            assert_eq!(world.phase(id), FilePhase::Sparsified);
            assert!(world.flag(id, FileFlag::HasSparse));
        }
        let records = world.db.get_records_by_file_id(id_bad).expect("records");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].file_id, Some(id_bad));
        assert_eq!(records[0].phase, ErrorPhase::Pipeline(crate::config::PipelinePhase::Sparsify));
    }

    #[test]
    fn modified_file_flagged() {
        let world = TestWorld::new();
        let stale = DateTime::from_timestamp(Utc::now().timestamp() - 3600, 0).expect("stale");
        // Recorded mtime is an hour old while the on-disk mtime is now:
        // warn_if_times_changed reports the drift.
        let id = world.add_file_recorded("a.bin", &zeros(4 * 4096), Some(stale), None, None);
        world.seed_dedup_row(id, 4);

        world.prepare_for_sparsify();
        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let (completed, _) = run_loop(&world, world.progress.thread_bar(0), 2);
        world.progress.drop_thread_bars();

        assert_eq!(completed, 1);
        assert!(world.flag(id, FileFlag::Modified));
        assert!(world.flag(id, FileFlag::HasSparse));
    }

    #[test]
    fn record_sparsify_error_persists_filestat() {
        let world = TestWorld::new();
        let id = world.add_file("e.bin", &zeros(4 * 4096));
        let mut recorder = Recorder::new(&world.db, true);
        record_sparsify_error(&mut recorder, &SparseOutcome {
            id,
            modified: false,
            err: Some(Error::FileStat(FileStatError::Io {
                path: world.path("e.bin"),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied, "denied".to_string()),
            })),
        });
        recorder.flush().expect("flush recorder");

        let records = world.db.get_records_by_file_id(id).expect("records");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].file_id, Some(id));
        assert_eq!(records[0].error_type, "Io/PermissionDenied");
        assert_eq!(records[0].phase, ErrorPhase::Pipeline(crate::config::PipelinePhase::Sparsify));
    }
}