use crate::archive::ArchiveRTArgs;
use crate::common::files::warn_if_times_changed;
use crate::common::{at_least_one_running, io_buffer};
use crate::db::Recorder;
use crate::db::flags::ErrorFlags;
use crate::db::hash::{HashError, HashSuccess, HashingOutcome};
use crate::db::types::StrippedRecord;
use crate::error::{Error, Result};
use crate::progress::BarKind;
use crate::shutdown::Shutdown;
use crossbeam_channel::{Receiver, Sender, bounded};
use indicatif::ProgressBar;
use sha1::{Digest, Sha1};
use std::fs::File;
use std::io::Read;
use std::mem::take;
use std::path::Path;
use std::thread;
use std::time::Duration;

// TODO via args
const BATCH_SIZE: u64 = 10_000;

// Producer/consumer pipeline bounds: the input queue takes over the old
// whole-batch pull's memory guard, but workers stream file-by-file so a big
// file no longer stalls every other row's commit and progress.
const WORK_CAPACITY: usize = BATCH_SIZE as usize;
const OUT_CAPACITY: usize = 2 * WORK_CAPACITY;
const FEED_CHUNK: usize = 1_024;      // rows pulled from the DB cursor per round
const DRAIN_CHUNK: usize = BATCH_SIZE as usize / 2;  // outcomes committed per transaction

pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    let db = rt.db;
    let shutdown = rt.shutdown;
    debug_assert!(rt.config.sparse.page_size > 0, "page_size == 0");
    let detect_hardlinks = !rt.config.indexing.no_hardlink_detection;
    let eager_filter = rt.config.filter.eager_filter;

    let total_entries = db.count_entries()?;
    let hash_needed = db.count_all_hashable_files(eager_filter, detect_hardlinks)?;
    let pending = db.count_pending_hashable_files(eager_filter, detect_hardlinks)?;
    let already_hashed = hash_needed.saturating_sub(pending);
    tracing::info!(
        total_entries,
        unshed_files = pending,
        already_hashed,
        jobs = rt.config.process.effective_jobs(),
        page_size = rt.config.sparse.page_size,
        "hash pass"
    );

    rt.progress.set_phase_total(hash_needed);
    rt.progress.set_phase_position(already_hashed);
    let promoted_entries = db.promote_unhasheable_files(eager_filter, detect_hardlinks)?;
    tracing::info!("Promoted {promoted_entries} entries which cannot be hashed.");
    rt.progress.inc_global(total_entries - hash_needed);

    if pending == 0 {
        return Ok(());
    }

    let progress = rt.progress;
    let page_size = rt.config.sparse.page_size;
    let jobs = rt.config.process.effective_jobs();

    // Ordering table: encodes size-DESC order (position column) so the feed
    // can pull pending rows in that order without a long-lived SQL cursor.
    // Populate is idempotent (INSERT OR IGNORE); dropped only on success.
    db.create_hash_queue()?;
    db.populate_hash_queue(eager_filter, detect_hardlinks)?;

    // One bar per worker, materialized now so each worker can take its bar by
    // value; workers never touch `ProgressBarSet`.
    progress.create_thread_bars(BarKind::Bytes, jobs);
    let mut bars = Vec::<ProgressBar>::new();
    for i in 0..jobs {
        bars.push(progress.thread_bar(i));
    }

    let (work_s, work_r) = bounded::<StrippedRecord>(WORK_CAPACITY);
    let (out_s, out_r) = bounded::<Option<HashingOutcome>>(OUT_CAPACITY);
    let mut thread_handles = Vec::with_capacity(jobs);

    for i in 0..jobs {
        let bar = bars[i].clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let sh = shutdown.clone();
        let ps = page_size;
        let res = thread::Builder::new()
            .name(format!("hash-worker-{i}").into())
            .spawn(move || hash_worker(bar, ps, sh, wr, os))
            .expect("spawn hash worker");
        thread_handles.push(res);
    }
    drop(work_r);
    drop(out_s);

    let is_running = || {
        at_least_one_running(&thread_handles.iter().collect())
    };

    let completed = handle_send_receive_loop(&rt, work_s, out_r, is_running)?;
    progress.drop_thread_bars();

    let double_canonical = db.count_double_canonical_dev_inode_group()?;
    if double_canonical > 0 {
        panic!("Encountered {double_canonical} Hard Link files. \
            which have two different hashes. Assuming files modified while hashing.");
    }

    match shutdown.is_interrupted() {
        true => {
            let msg = if shutdown.is_force() {
                "hashing force-aborted; in-flight progress discarded"
            } else {
                "hashing stopped; completed files saved"
            };
            tracing::warn!(saved = completed, "{msg}");
            Err(Error::Interrupted)
        }
        false => {
            db.drop_hash_queue()?;
            Ok(())
        }
    }
}

/// Perform the full enqueue / dequeue loop in batches to improve performance.
/// The state machine for the enqueue / dequeue process is quite involved and pollutes the name
/// space of the function, which is why it is moved to a separate function.
pub fn handle_send_receive_loop(
    rt: &ArchiveRTArgs,
    send: Sender<StrippedRecord>,
    recv: Receiver<Option<HashingOutcome>>,
    one_running: impl Fn() -> bool)
    -> Result<u64> {
    // Feed cursor over `hash_queue`: `queue_index` is the last consumed queue
    // position. The pull filters to still-pending rows, so a file already
    // handed to a worker (or hashed on a previous run) is never re-pulled.
    let mut recorder = Recorder::new(rt.db, !rt.config.process.no_errors);

    let mut queue_index = 0u64;
    let mut feed_buf = Vec::<(u64, StrippedRecord)>::new();
    let mut feed_idx = 0usize;
    let mut feed_exhausted = false;
    let mut busy = false;

    let mut dequeue_total = 0u64;
    let mut exited_workers = 0u64;
    let mut feed_total = 0u64;
    let mut completed = 0u64;
    let mut pending_out = Vec::<HashingOutcome>::new();

    let mut apply_chunk = |pending: &mut Vec<HashingOutcome>| -> Result<u64> {
        let items = take(pending);
        if items.is_empty() {
            return Ok(0);
        }
        let n = items.len() as u64;
        rt.db.ingest_hash_outcome(&items, !rt.config.indexing.no_hardlink_detection)?;
        for res in items {
            match res {
                Ok(_) => (),
                Err(e) => match &e.err {
                    Error::Interrupted => (),
                    Error::FileStat(_) => record_hash_error(&mut recorder, &e),
                    other => panic!(
                        "Invariant Error. Only FileStatError and Interrupted expected. Got: {other}"
                    ),
                } ,
            }
        }
        rt.progress.inc_both(n);
        Ok(n)
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
                Err(_) => break
            }
        }
        if pending_out.len() >= DRAIN_CHUNK || drain_override {
            completed += apply_chunk(&mut pending_out)?;
            *is_busy = true;
        }
        Ok(())
    };

    loop {
        // Any pending shutdown (graceful *or* force) stops the feed: workers
        // observe it between files / in-flight and either finish or abort, so
        // keeping the feed open would only pile up rows nobody consumes (and
        // on force could wedge the loop in a full-channel retry).
        if rt.shutdown.is_interrupted() { break; }
        busy = false;
        if feed_idx == feed_buf.len() {
            feed_buf = rt.db.pull_pending_hash_rows::<StrippedRecord>(
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
                Err(_) => break
            }
        }

        drain_chunk(&mut busy, false, &mut dequeue_total, &mut exited_workers)?;
        // Leave for dequeue loop.
        if feed_exhausted && feed_idx == feed_buf.len() {
            break;
        }
        if !one_running() {
            break;
        }
        if !busy {
            thread::sleep(Duration::from_millis(10));
        }
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
        busy = false;
        if dequeue_total == feed_total {
            break;
        }
        if !one_running() {
            break;
        }
        if exited_workers == rt.config.process.effective_jobs() as u64 {
            break;
        }
        drain_chunk(&mut busy, false, &mut dequeue_total, &mut exited_workers)?;
        if !busy {
            thread::sleep(Duration::from_millis(4));
        }
    }
    drain_chunk(&mut busy, true, &mut dequeue_total, &mut exited_workers)?;
    drop(recv);
    recorder.flush()?;
    Ok(completed)
}

/// Record a per-file hash failure in the persistent error log (best-effort).
/// `hash_file` failures are `FileStat` (per-file) errors; the carried
/// [`FileStatError`](crate::error::FileStatError) is recreated on the way in.
fn record_hash_error(recorder: &mut Recorder, e: &HashError) {
    recorder.record_file(
        e.id,
        crate::db::ErrorPhase::Pipeline(crate::config::PipelinePhase::Hash),
        e.err.to_file_stat(None), // TODO move this outside.
        ErrorFlags::default(),
    );
}
/// Worker thread: pulls one file at a time, hashes it, forwards the outcome.
/// Owns its bar and read buffer; only touches the channels and `tracing`.
fn hash_worker(
    bar: ProgressBar,
    page_size: usize,
    shutdown: Shutdown,
    work: Receiver<StrippedRecord>,
    out: Sender<Option<HashingOutcome>>) -> () {
    let mut buf = io_buffer();
    loop {
        // INFO: Destroy channel to exit the worker!
        match work.recv() {
            Ok(row) => {
                if shutdown.check_between_files().is_err() {
                    break;
                }
                bar.reset();
                bar.set_length(row.size);
                bar.set_message(format!("Hashing {}", row.abs_path.display()));
                let modified = warn_if_times_changed(
                    &row.abs_path, row.mtime, row.atime, row.ctime
                );
                let res = match hash_one(
                    &mut buf, &row.abs_path, page_size, &shutdown, Some(&bar)) {
                    Ok((hash, zero_pages)) => Ok(HashSuccess {
                        id: row.id, hash, zero_pages, modified
                    }),
                    Err(err) => Err(HashError {
                        id: row.id, err, modified
                    })
                };
                out.send(Some(res)).expect("hash worker: result channel closed");
            }
            Err(_) => break
        }
    }
    out.send(None).expect("hash worker: result channel closed");
}

/// Single-pass SHA-1 and empty-page count.
///
/// `buf` is the worker's reusable read buffer (sized once here); `pb`
/// advances live per buffer so a huge file's bar stays responsive.
///
/// Bytes are hashed as read. Separately, the stream is partitioned into fixed
/// `page_size` windows (independent of the I/O buffer). Only **full**
/// all-zero windows count; a short trailing window does not (same rule as
/// `sparse-cp::sparse_page_count`).
///
/// Zero checks slice `buf` in place. Across a read boundary we only keep
/// `carry_len` / `carry_zero` — never the leftover bytes themselves.
fn hash_one(
    buf: &mut Vec<u8>,
    path: &Path,
    page_size: usize,
    shutdown: &Shutdown,
    pb: Option<&ProgressBar>)
    -> Result<([u8; 20], u64)> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;

    let mut hasher = Sha1::new();
    let mut zero_blocks = 0u64;
    // Incomplete page spanning the previous read: length so far, and whether
    // those bytes were all zero. `carry_len > 0` is the "cut off by buffer" flag.
    let mut carry_len = 0usize;
    let mut carry_zero = true;

    loop {
        match shutdown.check_in_flight() {
            Ok(_) => (),
            Err(e) => {
                tracing::info!("hash worker got in-flight interrupt");
                return Err(e);
            }
        };
        let n = file.read(buf).map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);

        let mut i = 0usize;

        // Handle segmentation between read_bufs
        if carry_len > 0 {
            let need = page_size - carry_len;
            if n < need {
                carry_zero &= is_all_zero(&buf[..n]);
                carry_len += n;
                continue;
            }
            if carry_zero && is_all_zero(&buf[..need]) {
                zero_blocks += 1;
            }
            carry_len = 0;
            carry_zero = true;
            i = need;
        }

        // Scan contiguous buffer
        while i + page_size <= n {
            if is_all_zero(&buf[i..i + page_size]) {
                zero_blocks += 1;
            }
            i += page_size;
        }

        // Scan remaining page for zeros.
        let rem = n - i;
        if rem > 0 {
            carry_len = rem;
            carry_zero = is_all_zero(&buf[i..n]);
        }
        if let Some(pb) = pb {
            pb.inc(n as u64);
        }
    }

    Ok((hasher.finalize().into(), zero_blocks))
}

#[inline]
fn is_all_zero(chunk: &[u8]) -> bool {
    chunk.iter().all(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::IO_BUF_SIZE;
    use crate::common::files::original_extension;
    use crate::common::start::StartPolicy;
    use crate::config::{
        ArchiveConfig, ArchivePipelineOptions, CaptureOptions, CleanupSettings, CompressionFormat,
        CompressionSettings, FilterOptions, IndexingOptions, InputOptions, OwnerPolicy, PathLayout,
        PipelinePhase, ProcessOptions, SparseOptions,
    };
    use crate::db::flags::FileFlag;
    use crate::db::types::{FileId, FilePhase, FileType, NewFileRecord};
    use crate::db::{Database, ErrorPhase, Recorder};
    use crate::error::{Error, FileStatError};
    use crate::progress::ProgressBarSet;
    use chrono::{DateTime, Utc};
    use nix::unistd::geteuid;
    use rusqlite::named_params;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    fn pattern(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| ((i % 251) as u8) ^ seed).collect()
    }

    fn sha1_of(payload: &[u8]) -> [u8; 20] {
        let mut hasher = Sha1::new();
        hasher.update(payload);
        hasher.finalize().into()
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
                owner: None,
                owner_map: None,
                group: None,
                group_map: None,
            },
            sparse: SparseOptions { sparsify: false, page_size: 4096, min_pages: 0 },
            compression: CompressionSettings {
                format: CompressionFormat::None,
                level: 0,
                xz_extreme: false,
                memlimit_compress: None,
            },
            process: ProcessOptions {
                start_policy: StartPolicy::Create,
                jobs: 4,
                io_jobs: 4,
                fail_fast: false,
                no_errors: false,
                cleanup: CleanupSettings { keep_db: false, keep_stage: false },
                exit_after_stage: None,
            },
            pipeline: ArchivePipelineOptions {
                no_dedup: true,
                retry_missing_sha: false,
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
            Self {
                dir,
                db,
                shutdown: Shutdown::detached(),
                progress: ProgressBarSet::new(7),
                config: test_archive_config(),
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

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        fn add_file(&self, name: &str, payload: &[u8]) -> FileId {
            let path = self.dir.path().join(name);
            std::fs::write(&path, payload).expect("write test payload");
            self.insert_recorded(&path, payload.len(), None, None, None)
        }

        fn add_file_recorded(
            &self, name: &str, payload: &[u8],
            mtime: Option<DateTime<Utc>>, atime: Option<DateTime<Utc>>, ctime: Option<DateTime<Utc>>)
            -> FileId {
            let path = self.dir.path().join(name);
            std::fs::write(&path, payload).expect("write test payload");
            self.insert_recorded(&path, payload.len(), mtime, atime, ctime)
        }

        fn insert_recorded(
            &self, path: &Path, size: usize,
            mtime: Option<DateTime<Utc>>, atime: Option<DateTime<Utc>>, ctime: Option<DateTime<Utc>>)
            -> FileId {
            self.db.insert_file(&NewFileRecord {
                abs_path: PathBuf::from(path),
                ext: original_extension(path),
                size: size as u64,
                mtime, atime, ctime,
                uid: None, gid: None, mode: None,
                ftype: Some(FileType::File),
                xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None, link_dst: None,
                device_id: None, inode_id: None, major: None, minor: None,
            }).expect("insert test file");
            self.db.file_id_by_abs_path(path).expect("lookup test file").expect("file present")
        }

        fn sparse_count(&self, id: FileId) -> i64 {
            self.db.with_transaction(|conn| {
                let v: i64 = conn.query_row(
                    "SELECT sparse_count FROM files WHERE id = :id",
                    named_params! { ":id": id.0 },
                    |r| r.get(0),
                ).expect("read sparse_count");
                Ok(v)
            }).expect("sparse_count read")
        }

        fn hashed_digest(&self, id: FileId) -> Option<[u8; 20]> {
            self.db.get_file_by_id::<StrippedRecord>(id)
                .expect("get row")
                .expect("row present")
                .sha1
        }
    }

    /// One worker + one bar, driving `handle_send_receive_loop` to completion.
    fn run_loop(
        db: &Database, shutdown: &Shutdown, progress: &ProgressBarSet, config: &ArchiveConfig,
        bar: ProgressBar, work_cap: usize) -> u64 {
        let (work_s, work_r) = bounded::<StrippedRecord>(work_cap);
        let (out_s, out_r) = bounded::<Option<HashingOutcome>>(OUT_CAPACITY);
        let ps = config.sparse.page_size;
        let sh = shutdown.clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let mut handles = Vec::new();

        let thread = thread::Builder::new().name("hash-worker-test".into())
            .spawn(move || hash_worker(bar, ps, sh, wr, os))
            .expect("spawn hash worker");
        handles.push(&thread);
        drop(work_r);
        drop(out_s);
        let one_running = || { at_least_one_running(&handles) };
        let rt = ArchiveRTArgs { config, db, shutdown, progress };
        handle_send_receive_loop(&rt, work_s, out_r, one_running).expect("hash send/receive loop")
    }

    #[test]
    fn run_hashes_and_stores_digests() {
        let world = TestWorld::new();
        let big = pattern(1024 * 1024, 3);
        let tiny = pattern(7, 9);
        let empty: Vec<u8> = Vec::<u8>::new();
        let mut mixed = Vec::<u8>::new();
        mixed.resize_with(4 * 4096 + 1024, || 0u8);
        for (i, b) in pattern(1024, 5).iter().enumerate() {
            mixed[4 * 4096 + i] = *b;
        }

        let id_big = world.add_file("big.bin", &big);
        let id_dup = world.add_file("dup.bin", &big);
        let id_tiny = world.add_file("tiny.bin", &tiny);
        let id_empty = world.add_file("empty.bin", &empty);
        let id_mixed = world.add_file("mixed.bin", &mixed);

        run(&world.rt()).expect("hash run");

        for (id, payload) in [
            (id_big, &big),
            (id_dup, &big),
            (id_tiny, &tiny),
            (id_empty, &empty),
            (id_mixed, &mixed),
        ] {
            let row = world.db.get_file_by_id::<StrippedRecord>(id)
                .expect("get row").expect("row present");
            assert_eq!(row.phase, FilePhase::Hashed);
            assert_eq!(row.sha1, Some(sha1_of(payload)));
        }
        // the four full zero pages are counted, the short random tail is not.
        assert_eq!(world.sparse_count(id_mixed), 4);
        assert_eq!(world.sparse_count(id_big), 0);
    }

    #[test]
    fn unreadable_file_recorded_not_fatal() {
        // A 0o000 file is still readable by root; the test needs a real
        // permission failure.
        if geteuid().is_root() {
            return;
        }
        let world = TestWorld::new();
        let payload = pattern(64 * 1024, 1);
        let id_ok1 = world.add_file("ok1.bin", &payload);
        let id_bad = world.add_file("bad.bin", &payload);
        let id_ok2 = world.add_file("ok2.bin", &payload);

        let bad_path = world.path("bad.bin");
        std::fs::set_permissions(&bad_path, std::fs::Permissions::from_mode(0))
            .expect("chmod 000");

        run(&world.rt()).expect("hash run tolerates an unreadable file");

        assert_eq!(
            world.db.get_file_flag(id_bad, FileFlag::ErrorWhileHash).expect("flag"),
            true
        );
        if let Some(_) = world.hashed_digest(id_bad) {
            panic!("unreadable file must not get a digest");
        }
        assert_ne!(
            world.db.get_records_by_file_id(id_bad).expect("records").len(),
            0
        );
        for id in [id_ok1, id_ok2] {
            assert_eq!(world.hashed_digest(id), Some(sha1_of(&payload)));
            assert_eq!(
                world.db.get_file_by_id::<StrippedRecord>(id)
                    .expect("get row").expect("row").phase,
                FilePhase::Hashed
            );
        }
    }

    #[test]
    fn modified_file_flagged() {
        let world = TestWorld::new();
        let now = Utc::now();
        let stale = DateTime::from_timestamp(now.timestamp() - 3600, 0).expect("stale");
        let payload = pattern(128 * 1024, 4);
        let id = world.add_file_recorded("mod.bin", &payload, Some(stale), None, None);

        run(&world.rt()).expect("hash run");

        assert_eq!(
            world.db.get_file_flag(id, FileFlag::Modified).expect("flag"),
            true
        );
        assert_eq!(world.hashed_digest(id), Some(sha1_of(&payload)));
    }

    #[test]
    fn hash_order_is_size_desc() {
        let world = TestWorld::new();
        let id_small = world.add_file("s.bin", &pattern(1024 * 1024, 1));
        let id_mid = world.add_file("m.bin", &pattern(4 * 1024 * 1024 + 1, 2));
        let id_big = world.add_file("b.bin", &pattern(8 * 1024 * 1024, 3));

        world.db.create_hash_queue().expect("create queue");
        world.db.populate_hash_queue(false, false).expect("populate queue");

        let ids = world.db.pull_pending_hash_rows::<StrippedRecord>(0, 100)
            .expect("pull")
            .into_iter()
            .map(|pair| pair.1.id)
            .collect::<Vec<FileId>>();
        assert_eq!(ids, [id_big, id_mid, id_small].to_vec());
    }

    #[test]
    fn zero_page_counter_partial_last_page() {
        let world = TestWorld::new();
        let path = world.path("z1.bin");
        let payload = vec![0u8; 3 * 4096 + 4089];
        std::fs::write(&path, &payload).expect("write");
        let mut buf = io_buffer();
        let (_hash, sparse) = hash_one(&mut buf, &path, 4096, &Shutdown::detached(), None)
            .expect("hash");
        assert_eq!(sparse, 3);
    }

    #[test]
    fn zero_page_counter_exact_multiple() {
        let world = TestWorld::new();
        let path = world.path("z2.bin");
        let payload = vec![0u8; 40 * 4096];
        std::fs::write(&path, &payload).expect("write");
        let mut buf = io_buffer();
        let (_hash, sparse) = hash_one(&mut buf, &path, 4096, &Shutdown::detached(), None)
            .expect("hash");
        assert_eq!(sparse, 40);
    }

    #[test]
    fn zero_page_counter_mixed_content() {
        let world = TestWorld::new();
        let path = world.path("z3.bin");
        let mut payload = vec![0u8; 2 * 4096];
        for b in pattern(4096, 7) {
            payload.push(b);
        }
        std::fs::write(&path, &payload).expect("write");
        let mut buf = io_buffer();
        let (_hash, sparse) = hash_one(&mut buf, &path, 4096, &Shutdown::detached(), None)
            .expect("hash");
        assert_eq!(sparse, 2);
    }

    #[test]
    fn zero_page_counter_crosses_buffer_boundary() {
        let world = TestWorld::new();
        let path = world.path("z4.bin");
        let payload = vec![0u8; IO_BUF_SIZE + 2 * 4096 + 4089];
        std::fs::write(&path, &payload).expect("write");
        let mut buf = io_buffer();
        let (_hash, sparse) = hash_one(&mut buf, &path, 4096, &Shutdown::detached(), None)
            .expect("hash");
        assert_eq!(sparse, ((IO_BUF_SIZE + 2 * 4096) / 4096) as u64);
    }

    #[test]
    fn record_hash_error_persists_filestat() {
        let world = TestWorld::new();
        let id = world.add_file("e.bin", &pattern(1024, 1));
        let mut recorder = Recorder::new(&world.db, true);
        record_hash_error(&mut recorder, &HashError {
            id,
            modified: false,
            err: Error::FileStat(FileStatError::Io {
                path: PathBuf::from("/tmp/e.bin"),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied, "nope".to_string()),
            }),
        });
        recorder.flush().expect("flush recorder");

        let records = world.db.get_records_by_file_id(id).expect("records");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].file_id, Some(id));
        assert_eq!(records[0].error_type, "Io/PermissionDenied");
        assert_eq!(records[0].phase, ErrorPhase::Pipeline(PipelinePhase::Hash));
    }

    #[test]
    fn final_drain_applies_partial_batch_no_loss() {
        let world = TestWorld::new();
        let mut expected = Vec::<(FileId, [u8; 20])>::new();
        for i in 0..3 {
            let payload = pattern(1024 * 1024 + i, i as u8);
            let name = format!("f{i}.bin");
            let id = world.add_file(name.as_str(), &payload);
            expected.push((id, sha1_of(&payload)));
        }
        world.db.create_hash_queue().expect("create queue");
        world.db.populate_hash_queue(false, false).expect("populate queue");

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let completed = run_loop(
            &world.db, &world.shutdown, &world.progress, &world.config,
            world.progress.thread_bar(0), 2);
        world.progress.drop_thread_bars();

        assert_eq!(completed, 3);
        for (id, digest) in expected {
            assert_eq!(world.hashed_digest(id), Some(digest));
        }
    }

    #[test]
    fn interrupt_mid_feed_pauses_and_resume_completes() {
        let world = TestWorld::new();
        let mut expected = Vec::<(FileId, [u8; 20])>::new();
        for i in 0..8 {
            let payload = pattern(8 * 1024 * 1024 + i, i as u8);
            let name = format!("f{i}.bin");
            let id = world.add_file(name.as_str(), &payload);
            expected.push((id, sha1_of(&payload)));
        }
        world.db.create_hash_queue().expect("create queue");
        world.db.populate_hash_queue(false, false).expect("populate queue");

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh2 = world.shutdown.clone();
        let bar_obs = bar.clone();
        let trigger = thread::spawn(move || {
            // Fire once the worker completed a whole file (position went > 0
            // and reset to 0 between files) — at least one outcome is already
            // in-hand, and the worker is never mid-file when the loop sees us.
            let mut started = false;
            for _ in 0..20_000 {
                if started && bar_obs.position() == 0 {
                    sh2.request_graceful();
                    return;
                }
                if bar_obs.position() > 0 {
                    started = true;
                }
                thread::sleep(Duration::from_millis(1));
            }
            sh2.request_graceful();
        });

        let completed = run_loop(
            &world.db, &world.shutdown, &world.progress, &world.config, bar, 2);
        trigger.join().expect("join trigger");
        world.progress.drop_thread_bars();

        assert!(completed >= 1);
        assert!(completed < 8);
        let pending_remaining = world.db
            .count_pending_hashable_files(false, false).expect("pending");
        assert_eq!(pending_remaining, 8 - completed);
        // interrupt/resume keeps the ordering table (drop happens on success only)
        let queue_alive = world.db.pull_pending_hash_rows::<StrippedRecord>(0, 100)
            .expect("queue survives interrupt");
        assert_ne!(queue_alive.len(), 0);

        // Resume with a fresh shutdown: everything still pending is hashed.
        let sh3 = Shutdown::detached();
        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let completed2 = run_loop(
            &world.db, &sh3, &world.progress, &world.config,
            world.progress.thread_bar(0), 2);
        world.progress.drop_thread_bars();

        assert_eq!(completed + completed2, 8);
        for (id, digest) in expected {
            assert_eq!(world.hashed_digest(id), Some(digest));
        }
    }

    #[test]
    fn interrupt_dequeue_only_finishes_in_flight() {
        let world = TestWorld::new();
        for i in 0..3 {
            let payload = pattern(16 * 1024 * 1024 + i, i as u8);
            let name = format!("g{i}.bin");
            world.add_file(name.as_str(), &payload);
        }
        world.db.create_hash_queue().expect("create queue");
        world.db.populate_hash_queue(false, false).expect("populate queue");

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let (work_s, work_r) = bounded::<StrippedRecord>(2);
        let (out_s, out_r) = bounded::<Option<HashingOutcome>>(OUT_CAPACITY);
        let sh = world.shutdown.clone();
        let ps = world.config.sparse.page_size;
        let wbar = bar.clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        thread::Builder::new().name("hash-worker-test".into())
            .spawn(move || hash_worker(wbar, ps, sh, wr, os))
            .expect("spawn hash worker");
        drop(work_r);
        drop(out_s);

        let sh2 = world.shutdown.clone();
        let bar_obs = bar.clone();
        let trigger = thread::spawn(move || {
            // All fed rows are already in the worker's hands (the cap-2 channel
            // drained into the worker) and one full file completed: graceful
            // must finish the in-flight file but drop anything not started yet.
            let mut started = false;
            for _ in 0..20_000 {
                if started && bar_obs.position() == 0 {
                    sh2.request_graceful();
                    return;
                }
                if bar_obs.position() > 0 {
                    started = true;
                }
                thread::sleep(Duration::from_millis(1));
            }
            sh2.request_graceful();
        });
        let handles = Vec::from([&trigger]);

        let one_running = || at_least_one_running(&handles);
        let completed = handle_send_receive_loop(
            &world.rt(), work_s, out_r, one_running).expect("hash send/receive loop");
        trigger.join().expect("join trigger");
        world.progress.drop_thread_bars();

        // Graceful stop: at least the completed file's outcome survived.
        assert!(completed >= 1);
        let pending_remaining = world.db
            .count_pending_hashable_files(false, false).expect("pending");
        assert_eq!(pending_remaining, 3 - completed);

        let sh3 = Shutdown::detached();
        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let completed2 = run_loop(
            &world.db, &sh3, &world.progress, &world.config,
            world.progress.thread_bar(0), 2);
        world.progress.drop_thread_bars();
        assert_eq!(completed + completed2, 3);
        assert_eq!(
            world.db.count_pending_hashable_files(false, false).expect("pending"),
            0
        );
    }

    #[test]
    fn triple_interrupt_force_discards_in_flight() {
        let world = TestWorld::new();
        let payload = pattern(32 * 1024 * 1024, 11);
        let id = world.add_file("huge.bin", &payload);
        world.db.create_hash_queue().expect("create queue");
        world.db.populate_hash_queue(false, false).expect("populate queue");

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let (work_s, work_r) = bounded::<StrippedRecord>(2);
        let (out_s, out_r) = bounded::<Option<HashingOutcome>>(OUT_CAPACITY);
        let sh = world.shutdown.clone();
        let ps = world.config.sparse.page_size;
        let wbar = bar.clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        thread::Builder::new().name("hash-worker-test".into())
            .spawn(move || hash_worker(wbar, ps, sh, wr, os))
            .expect("spawn hash worker");
        drop(work_r);
        drop(out_s);

        let sh2 = world.shutdown.clone();
        let bar_obs = bar.clone();
        let trigger = thread::spawn(move || {
            // Fire mid-read: the worker aborts at the next in-flight check.
            for _ in 0..20_000 {
                if bar_obs.position() > 0 {
                    sh2.request_force();
                    return;
                }
                thread::sleep(Duration::from_millis(1));
            }
            sh2.request_force();
        });
        let handles = Vec::from([&trigger]);

        let completed = handle_send_receive_loop(
            &world.rt(), work_s, out_r,
            || at_least_one_running(&handles)).expect("hash send/receive loop");
        trigger.join().expect("join trigger");
        world.progress.drop_thread_bars();

        // The interrupted outcome was processed (counted) but must not persist
        // anything: no digest, no error flag, no error log row. (`completed`
        // can be 0 if the abort landed on a between-files check instead of
        // inside the read; both states still discard the in-flight file.)
        assert!(completed <= 1);
        if let Some(_) = world.hashed_digest(id) {
            panic!("force-aborted hash must not commit a digest");
        }
        assert_eq!(
            world.db.get_file_flag(id, FileFlag::ErrorWhileHash).expect("flag"),
            false
        );
        assert_eq!(
            world.db.get_records_by_file_id(id).expect("records").len(),
            0
        );
        // the row is still pending and the queue survives for the resume
        assert_eq!(
            world.db.count_pending_hashable_files(false, false).expect("pending"),
            1
        );
        let sh3 = Shutdown::detached();
        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let completed2 = run_loop(
            &world.db, &sh3, &world.progress, &world.config,
            world.progress.thread_bar(0), 2);
        world.progress.drop_thread_bars();
        assert_eq!(completed2, 1);
        assert_eq!(world.hashed_digest(id), Some(sha1_of(&payload)));
    }

    #[test]
    fn run_force_before_start_returns_interrupted() {
        let world = TestWorld::new();
        let payload = pattern(1024 * 1024, 1);
        let id = world.add_file("a.bin", &payload);
        world.shutdown.request_force();

        let res = run(&world.rt());

        assert!(matches!(res, Err(Error::Interrupted)));
        if let Some(_) = world.hashed_digest(id) {
            panic!("force-aborted hash run must not commit a digest");
        }
        // the ordering table survives for the resume
        let _ = world.db.pull_pending_hash_rows::<StrippedRecord>(0, 100)
            .expect("queue survives");
    }

    #[test]
    #[ignore]
    fn batch_size_parity_stub() {
        // TODO(batch_size): once a `--batch_size` knob exists, slice the same
        // DB with different batch sizes and assert identical result order.
        let world = TestWorld::new();
        for i in 0..5 {
            let payload = pattern(1024, i as u8);
            let name = format!("b{i}.bin");
            world.add_file(name.as_str(), &payload);
        }
        world.db.create_hash_queue().expect("create queue");
        world.db.populate_hash_queue(false, false).expect("populate queue");
        let all = world.db.pull_pending_hash_rows::<StrippedRecord>(0, 100).expect("pull all");
        let mut collected = Vec::<u64>::new();
        let mut pos = 0u64;
        loop {
            let batch = world.db.pull_pending_hash_rows::<StrippedRecord>(pos, 2)
                .expect("pull batch");
            if batch.is_empty() {
                break;
            }
            for pair in batch {
                let (p, _row) = pair;
                collected.push(p);
                pos = p;
            }
        }
        assert_eq!(collected, all.into_iter().map(|pair| pair.0).collect::<Vec<u64>>());
    }
}
