//! Rehash: verify extract-cache payloads against catalog SHA-1 digests.

use crate::common::io_buffer;
use crate::common::send_receive_loop;
use crate::config::ExtractPipelinePhase;
use crate::db::flags::ErrorFlags;
use crate::db::rehash::RehashOutcome;
use crate::db::types::StrippedRecord;
use crate::db::{ErrorPhase, Recorder};
use crate::error::{Error, Result};
use crate::progress::BarKind;
use crate::shutdown::Shutdown;
use crate::unarchive::ExtractRTArgs;
use crossbeam_channel::{Receiver, Sender, bounded};
use indicatif::ProgressBar;
use sha1::{Digest, Sha1};
use std::fs::File;
use std::io::Read;
use std::mem::take;
use std::path::{Path, PathBuf};
use std::thread;

// TODO via args
const BATCH_SIZE: u64 = 10_000;

// Producer/consumer pipeline bounds: the input queue takes over the old
// whole-batch pull's memory guard, but workers stream file-by-file so a big
// file no longer stalls every other row's commit and progress.
const WORK_CAPACITY: usize = BATCH_SIZE as usize;
const OUT_CAPACITY: usize = 2 * WORK_CAPACITY;
const FEED_CHUNK: usize = 1_024;                     // rows pulled from the DB per round
const DRAIN_CHUNK: usize = BATCH_SIZE as usize / 2;  // outcomes committed per transaction

const ERROR_PHASE: ErrorPhase = ErrorPhase::Extract(ExtractPipelinePhase::Rehash);

/// Applied-outcome tally kept by the send/receive loop; `ingest_rehash_outcome`
/// persists flags + phase in a transaction, the error-log rows are pushed into
/// the caller-side `Recorder`.
struct RehashCounts {
    pub matches: u64,
    pub mismatches: u64,
    pub errors: u64,
}

pub fn run(rt: &ExtractRTArgs) -> Result<()> {
    let total = rt.db.count_files_to_rehash()?;
    let done = rt.db.count_rehashed_files()?;
    let pending = total.saturating_sub(done);
    let do_skip = if rt.config.scan.rehash { "" } else { "skip " };
    tracing::info!(
        files = total,
        pending,
        jobs = rt.config.process.effective_jobs(),
        "{do_skip}rehash pass"
    );

    if !rt.config.scan.rehash {
        let n = rt.db.skip_rehash()?;
        rt.progress.inc_global(n);
        tracing::info!(promoted = n, "rehash skipped; extract_filtered → rehashed");
        return Ok(());
    }

    rt.progress.set_phase_total(total);
    rt.progress.set_phase_position(done);
    // Non-elected rows (dupes, non-files, filter-excluded, sha-less) skip the
    // queue entirely and are promoted first; only elected rows are verified.
    let promoted = rt.db.promote_unrehashable_files()?;
    tracing::info!("Promoted {promoted} entries which cannot be rehashed.");
    rt.progress.inc_global(promoted);

    if pending == 0 {
        return Ok(());
    }

    // Ordering table: encodes size-DESC order (position column) so the feed
    // can pull pending rows in that order without a long-lived SQL cursor.
    // Populate is idempotent (INSERT OR IGNORE); dropped only on success.
    rt.db.create_rehash_queue()?;
    rt.db.populate_rehash_queue()?;

    // One bar per worker, materialized now so each worker can take its bar by
    // value; workers never touch `ProgressBarSet`.
    rt.progress.create_thread_bars(BarKind::Bytes, rt.config.process.effective_jobs());
    let mut bars = Vec::<ProgressBar>::new();
    for i in 0..rt.config.process.effective_jobs() {
        bars.push(rt.progress.thread_bar(i));
    }

    let (work_s, work_r) = bounded::<StrippedRecord>(WORK_CAPACITY);
    let (out_s, out_r) = bounded::<Option<RehashOutcome>>(OUT_CAPACITY);
    let mut thread_handles = Vec::with_capacity(rt.config.process.effective_jobs());

    for i in 0..rt.config.process.effective_jobs() {
        let bar = bars[i].clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let sh = rt.shutdown.clone();
        let sd = rt.config.paths.stage_dir().clone();
        let res = thread::Builder::new()
            .name(format!("rehash-worker-{i}").into())
            .spawn(move || rehash_worker(sd, bar, sh, wr, os))
            .expect("spawn rehash worker");
        thread_handles.push(res);
    }
    drop(work_r);
    drop(out_s);

    let counts = handle_send_receive_loop(rt, work_s, out_r, thread_handles)?;
    rt.progress.drop_thread_bars();

    match rt.shutdown.is_interrupted() {
        true => {
            let msg = if rt.shutdown.is_force() {
                "rehash force-aborted; in-flight progress discarded"
            } else {
                "rehash stopped; completed files saved"
            };
            tracing::warn!(
                saved = counts.matches + counts.mismatches + counts.errors,
                "{msg}"
            );
            Err(Error::Interrupted)
        }
        false => {
            rt.db.drop_rehash_queue()?;
            tracing::info!(
                matches = counts.matches,
                mismatches = counts.mismatches,
                errors = counts.errors,
                "rehash complete"
            );
            if counts.mismatches > 0 {
                if rt.config.force {
                    tracing::warn!(mismatches = counts.mismatches, "rehash digest mismatch(es) recorded");
                } else {
                    // TODO different error
                    return Err(Error::Config(format!(
                        "Corruption detected: {} files with mismatching hash. \
                        Ignore this error with --force",
                        counts.mismatches
                    )));
                }
            }
            if counts.errors > 0 {
                if rt.config.process.fail_fast {
                    // TODO different error
                    return Err(Error::Config(format!(
                        "Encountered {} errors while rehashing.",
                        counts.errors
                    )));
                }
                tracing::warn!(counts.errors, "rehash error(s) recorded");
            }
            Ok(())
        }
    }
}

/// Perform the full enqueue / dequeue loop in batches to improve performance.
/// The state machine for the enqueue / dequeue process is quite involved and
/// pollutes the name space of the function, which is why it is moved to a
/// separate function. Owns the worker handles: the shared loop joins them
/// before the final drain so a trailing `None` can never race `drop(recv)`.
fn handle_send_receive_loop(
    rt: &ExtractRTArgs,
    send: Sender<StrippedRecord>,
    recv: Receiver<Option<RehashOutcome>>,
    thread_handles: Vec<thread::JoinHandle<()>>)
    -> Result<RehashCounts> {
    let mut recorder = Recorder::new(rt.db, !rt.config.process.no_errors);
    let mut counts = RehashCounts { matches: 0, mismatches: 0, errors: 0 };

    // Feed cursor over `rehash_queue`: `queue_index` is the last consumed queue
    // position. The pull filters to still-pending rows (phase predicate), so a
    // file already handed to a worker (or rehashed on a previous run) is never
    // re-pulled.
    let mut queue_index = 0u64;
    let pull = || {
        let rows = rt.db.pull_pending_rehash_rows::<StrippedRecord>(
            queue_index, FEED_CHUNK as u64)?;
        if !rows.is_empty() {
            queue_index = rows[rows.len() - 1].0;
        }
        Ok(rows.into_iter().map(|(_q, row)| row).collect::<Vec<_>>())
    };
    let apply = |pending: &mut Vec<RehashOutcome>| -> Result<()> {
        let items = take(pending);
        if items.is_empty() {
            return Ok(());
        }
        let n = items.len() as u64;
        rt.db.ingest_rehash_outcome(&items)?;
        for outcome in items.iter() {
            match outcome {
                RehashOutcome::Match(_) => counts.matches += 1,
                RehashOutcome::Mismatch(_) => counts.mismatches += 1,
                RehashOutcome::Errored(id, fse) => {
                    recorder.record_file(
                        *id,
                        ERROR_PHASE,
                        fse.recreate(),
                        ErrorFlags::default(),
                    );
                    counts.errors += 1;
                }
            }
        }
        rt.progress.inc_both(n);
        Ok(())
    };

    let _ = send_receive_loop(
        rt.shutdown, send, recv, thread_handles,
        DRAIN_CHUNK, false,
        pull, |_| Ok(()), || Ok(true), apply)?;
    recorder.flush()?;
    Ok(counts)
}

/// Worker thread: pulls one file at a time, hashes it against the catalog
/// digest, forwards the outcome. Owns its bar; only touches the channels and
/// `tracing`. A force-abort mid-file drops the in-flight outcome (`None`), so
/// the row stays `extract_filtered` for the resume.
fn rehash_worker(
    stage_dir: PathBuf,
    bar: ProgressBar,
    shutdown: Shutdown,
    work: Receiver<StrippedRecord>,
    out: Sender<Option<RehashOutcome>>) -> () {
    let mut buf = io_buffer();
    loop {
        // INFO: Destroy channel to exit the worker!
        match work.recv() {
            Ok(row) => {
                if shutdown.is_interrupted() {
                    break;
                }
                bar.reset();
                bar.set_length(row.size);
                bar.set_message(format!("Rehashing {}", row.abs_path.display()));
                match rehash_one(&mut buf, &stage_dir, &row, &shutdown, Some(&bar)) {
                    Some(outcome) => {
                        out.send(Some(outcome)).expect("rehash worker: result channel closed");
                    }
                    None => break // Interrupted internally.
                }
            }
            Err(_) => break
        }
    }
    out.send(None).expect("rehash worker: result channel closed");
}

/// Compute and compare the hash of a single canonical record. Returns `None`
/// when the run was force-aborted mid-file: the in-flight row must not be
/// committed, mirroring how `archive/hash.rs` discards interrupted outcomes.
fn rehash_one(
    buf: &mut Vec<u8>,
    stage_dir: &Path,
    record: &StrippedRecord,
    shutdown: &Shutdown,
    pb: Option<&ProgressBar>)
    -> Option<RehashOutcome> {

    let member = record
        .tar_member_name()
        .expect("Archived files need to have a tar_member_name");

    let path = stage_dir.join(&member);
    let digest = match hash_file(&path, buf, shutdown, pb) {
        Ok(d) => d,
        // Force abort landed inside the read loop: keep the row pending.
        Err(Error::Interrupted) => return None,
        Err(e @ Error::FileStat(_)) => {
            tracing::warn!(
                file_id = record.id.0,
                path = %path.display(),
                error = %e,
                "rehash failed"
            );
            return Some(RehashOutcome::Errored(record.id, e.to_only_file_stat()?));
        }
        Err(e) => panic!(
            "INVARIANT ERROR: Unexpected error {e}. Expected Interrupted and FileStat"
        ),
    };
    let expected = record.sha1
        .expect("PRECONDITION FAILED: rehash requires sha1 to be present");

    if digest == expected {
        Some(RehashOutcome::Match(record.id))
    } else {
        Some(RehashOutcome::Mismatch(record.id))
    }
}

/// SHA-1 only (no sparse / hole accounting); advances the worker's byte bar.
fn hash_file(path: &Path, buf: &mut Vec<u8>, shutdown: &Shutdown, pb: Option<&ProgressBar>)
    -> Result<[u8; 20]> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
    let mut hasher = Sha1::new();

    loop {
        shutdown.check_in_flight()?;
        let n = file.read(buf).map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        if let Some(pb) = pb {
            pb.inc(n as u64);
        }
    }

    Ok(hasher.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::files::original_extension;
    use crate::config::ExtractConfig;
    use crate::db::Database;
    use crate::db::flags::FileFlag;
    use crate::db::types::{FileId, FilePhase, FileType, NewFileRecord};
    use crate::error::FileStatError;
    use crate::progress::{EXTRACT_MULTIPLIER, ProgressBarSet};
    use rusqlite::named_params;
    use std::time::Duration;

    fn pattern(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| ((i % 251) as u8) ^ seed).collect()
    }

    fn sha1_of(payload: &[u8]) -> [u8; 20] {
        let mut hasher = Sha1::new();
        hasher.update(payload);
        hasher.finalize().into()
    }

    struct ExtractTestWorld {
        dir: tempfile::TempDir,
        db: Database,
        shutdown: Shutdown,
        progress: ProgressBarSet,
        config: ExtractConfig,
    }

    impl ExtractTestWorld {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let work_dir = dir.path().join("estage");
            std::fs::create_dir_all(&work_dir).expect("create work dir");
            // `for_scan_test` defaults `force = true`, jobs = io_jobs = 1,
            // `scan.rehash = true`; tests override as needed.
            let config = ExtractConfig::for_scan_test(
                dir.path().join("archive.tar"),
                work_dir,
                dir.path().join("out"),
            );
            let db = Database::open(&config.paths.db_path()).expect("open db");
            Self {
                dir,
                db,
                shutdown: Shutdown::detached(),
                progress: ProgressBarSet::new(EXTRACT_MULTIPLIER),
                config,
            }
        }

        fn rt(&self) -> ExtractRTArgs<'_> {
            ExtractRTArgs {
                config: &self.config,
                db: &self.db,
                shutdown: &self.shutdown,
                progress: &self.progress,
            }
        }

        fn rt_with<'a>(&'a self, shutdown: &'a Shutdown) -> ExtractRTArgs<'a> {
            ExtractRTArgs {
                config: &self.config,
                db: &self.db,
                shutdown,
                progress: &self.progress,
            }
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.path().join(name)
        }

        /// Seed the joint `-1` archive+extract include rules so rows pass
        /// `generate_archive_and_extract_filter`.
        fn seed_filter(&self) {
            for table in ["archive", "extract"] {
                self.db.with_transaction(|conn| {
                    conn.execute(
                        &format!(
                            "INSERT OR IGNORE INTO filter_reason_{table} (id, source, line, expression) \
                             VALUES (-1, 'internal', NULL, '.*')"
                        ),
                        [],
                    ).expect("seed internal include rule");
                    Ok(())
                }).expect("seed filter tx");
            }
        }

        /// Elected canonical row: self-canonical `extract_filtered` file with a
        /// digest + `FileExtracted`, joint filter passed, payload written to the
        /// stage dir (== work dir for extract) under its member name.
        fn add_elected(&self, name: &str, payload: &[u8]) -> FileId {
            self.seed_filter();
            let seed_path = self.path(name);
            self.db.insert_file(&NewFileRecord {
                abs_path: seed_path.clone(),
                ext: original_extension(&seed_path),
                size: payload.len() as u64,
                mtime: None, atime: None, ctime: None,
                uid: None, gid: None, mode: None,
                ftype: Some(FileType::File),
                xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None, link_dst: None,
                device_id: None, inode_id: None, major: None, minor: None,
            }).expect("insert test file");
            let id = self.db.file_id_by_abs_path(&seed_path)
                .expect("lookup test file").expect("file present");
            // NOTE: mark_self_canonical sets phase='deduped', and
            // update_file_inspection_per_id sets phase='hashed'; the phase is
            // re-pinned to extract_filtered last, deliberately.
            self.db.mark_self_canonical(id).expect("self canonical");
            self.db.set_file_flag(id, FileFlag::FileExtracted, true).expect("extracted flag");
            self.db.update_file_inspection_per_id(id, sha1_of(payload), 0, false)
                .expect("set sha1");
            self.db.mark_file_phase(id, FilePhase::ExtractFiltered).expect("phase");
            self.db.with_transaction(|conn| {
                conn.execute(
                    "UPDATE files SET include_reason_archive = -1, exclude_reason_archive = 0, \
                     include_reason_extract = -1, exclude_reason_extract = 0 WHERE id = :id",
                    named_params! { ":id": id.0 },
                ).expect("set reason columns");
                Ok(())
            }).expect("reason tx");

            let row = self.db.get_file_by_id::<StrippedRecord>(id)
                .expect("get row").expect("row present");
            let member = row.tar_member_name().expect("tar member");
            std::fs::write(self.config.paths.stage_dir().join(&member), payload)
                .expect("write stage payload");
            id
        }

        /// A non-elected `extract_filtered` row (canonical != id) — swept by promote.
        fn add_dup(&self, name: &str, canonical: FileId) -> FileId {
            self.seed_filter();
            let seed_path = self.path(name);
            self.db.with_transaction(|conn| {
                conn.execute(
                    "INSERT INTO files (abs_path, ext, size, ftype, phase, sha1, \
                     include_reason_archive, exclude_reason_archive, \
                     include_reason_extract, exclude_reason_extract, flags, canonical_id) \
                     VALUES (:abs, '.bin', 0, 'file', 'extract_filtered', :sha1, -1, 0, -1, 0, :flag, :canon)",
                    named_params! {
                        ":abs": seed_path.to_string_lossy().into_owned(),
                        ":sha1": [7u8; 20].as_slice(),
                        ":flag": FileFlag::FileExtracted.mask_i64(),
                        ":canon": canonical.0,
                    },
                ).expect("insert dup");
                Ok(())
            }).expect("dup tx");
            self.db.file_id_by_abs_path(&seed_path)
                .expect("lookup test file").expect("file present")
        }

        /// A non-elected `extract_filtered` `dir` row — swept by promote.
        fn add_non_file(&self, name: &str) -> FileId {
            self.seed_filter();
            let seed_path = self.path(name);
            self.db.with_transaction(|conn| {
                conn.execute(
                    "INSERT INTO files (abs_path, ext, size, ftype, phase, \
                     include_reason_archive, exclude_reason_archive, \
                     include_reason_extract, exclude_reason_extract, flags) \
                     VALUES (:abs, '.bin', 0, 'dir', 'extract_filtered', -1, 0, -1, 0, 0)",
                    named_params! {
                        ":abs": seed_path.to_string_lossy().into_owned(),
                    },
                ).expect("insert non-file");
                Ok(())
            }).expect("non-file tx");
            self.db.file_id_by_abs_path(&seed_path)
                .expect("lookup test file").expect("file present")
        }

        /// The `run()` preamble minus bars and worker spawn: promote + ordering
        /// queue. Lets loop-level tests reuse `run`'s DB state.
        fn prepare_for_rehash(&self) {
            self.db.promote_unrehashable_files().expect("promote");
            self.db.create_rehash_queue().expect("create queue");
            self.db.populate_rehash_queue().expect("populate queue");
        }

        fn record(&self, id: FileId) -> StrippedRecord {
            self.db.get_file_by_id::<StrippedRecord>(id)
                .expect("get row").expect("row present")
        }

        fn phase(&self, id: FileId) -> FilePhase {
            self.record(id).phase
        }

        fn flag(&self, id: FileId, flag: FileFlag) -> bool {
            self.db.get_file_flag(id, flag).expect("get flag")
        }

        fn digest(&self, id: FileId) -> Option<[u8; 20]> {
            self.record(id).sha1
        }

        fn stage_path(&self, id: FileId) -> PathBuf {
            let member = self.record(id).tar_member_name().expect("tar member");
            self.config.paths.stage_dir().join(member)
        }

        fn errors(&self, id: FileId) -> Vec<crate::db::ErrorRecord> {
            self.db.get_records_by_file_id(id).expect("records")
        }

        fn queue_table_exists(&self) -> bool {
            self.db.with_transaction(|conn| {
                let n: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_master \
                     WHERE type = 'table' AND name = 'rehash_queue'",
                    [],
                    |row| row.get(0),
                ).expect("queue probe");
                Ok(n)
            }).expect("queue probe tx") > 0
        }
    }

    /// One worker + one bar, driving `handle_send_receive_loop` (the rehash
    /// wrapper over the shared loop). Returns `RehashCounts`.
    fn run_loop(rt: &ExtractRTArgs, bar: ProgressBar, work_cap: usize) -> RehashCounts {
        let (work_s, work_r) = bounded::<StrippedRecord>(work_cap);
        let (out_s, out_r) = bounded::<Option<RehashOutcome>>(OUT_CAPACITY);
        let sh = rt.shutdown.clone();
        let sd = rt.config.paths.stage_dir().clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let mut handles = Vec::<thread::JoinHandle<()>>::new();
        let worker = thread::Builder::new().name("rehash-worker-test".into())
            .spawn(move || rehash_worker(sd, bar, sh, wr, os))
            .expect("spawn rehash worker");
        handles.push(worker);
        drop(work_r);
        drop(out_s);
        handle_send_receive_loop(rt, work_s, out_r, handles)
            .expect("rehash send/receive loop")
    }

    #[test]
    fn hash_file_returns_sha1() {
        let world = ExtractTestWorld::new();
        let payload = pattern(256 * 1024, 1);
        let path = world.path("h1.bin");
        std::fs::write(&path, &payload).expect("write");
        let digest = hash_file(&path, &mut io_buffer(), &Shutdown::detached(), None)
            .expect("hash");
        assert_eq!(digest, sha1_of(&payload));
    }

    #[test]
    fn hash_file_open_error_propagates() {
        let world = ExtractTestWorld::new();
        let res = hash_file(&world.path("missing.bin"), &mut io_buffer(), &Shutdown::detached(), None);
        assert!(matches!(res, Err(Error::FileStat(_))));
    }

    #[test]
    fn hash_file_force_interrupts() {
        let world = ExtractTestWorld::new();
        let payload = pattern(1024, 2);
        let path = world.path("h3.bin");
        std::fs::write(&path, &payload).expect("write");
        let sh = Shutdown::detached();
        sh.request_force();
        let res = hash_file(&path, &mut io_buffer(), &sh, None);
        assert!(matches!(res, Err(Error::Interrupted)));
    }

    #[test]
    fn hash_file_graceful_continues() {
        let world = ExtractTestWorld::new();
        let payload = pattern(1024 * 1024, 3);
        let path = world.path("h4.bin");
        std::fs::write(&path, &payload).expect("write");
        let sh = Shutdown::detached();
        sh.request_graceful();
        let digest = hash_file(&path, &mut io_buffer(), &sh, None).expect("hash");
        assert_eq!(digest, sha1_of(&payload));
    }

    #[test]
    fn hash_file_pb_advances_bytes() {
        let world = ExtractTestWorld::new();
        let payload = pattern(1024 * 1024, 4);
        let path = world.path("h5.bin");
        std::fs::write(&path, &payload).expect("write");
        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let pb = world.progress.thread_bar(0);
        hash_file(&path, &mut io_buffer(), &Shutdown::detached(), Some(&pb))
            .expect("hash");
        assert!(pb.position() > 0, "bar must advance while hashing");
        world.progress.drop_thread_bars();
    }

    #[test]
    fn rehash_one_panics_on_missing_member() {
        let world = ExtractTestWorld::new();
        let payload = pattern(1024, 5);
        // A self-canonical row resolves a member name; a dup pointed elsewhere
        // (canonical != id) does not — the `expect` must fire for it.
        let id_canon = world.add_elected("m1c.bin", &payload);
        let id = world.add_dup("m1.bin", id_canon);
        let row = world.record(id);
        assert!(row.tar_member_name().is_none(), "dup must not resolve a member");
        let res = std::panic::catch_unwind(|| {
            rehash_one(&mut io_buffer(), &world.config.paths.stage_dir(), &row,
                       &Shutdown::detached(), None)
        });
        assert!(res.is_err(), "missing tar member must panic");
    }

    #[test]
    fn rehash_one_match_and_mismatch() {
        let world = ExtractTestWorld::new();
        let payload = pattern(4 * 4096, 7);
        let id_match = world.add_elected("m2a.bin", &payload);
        let row = world.record(id_match);
        let outcome = rehash_one(
            &mut io_buffer(), &world.config.paths.stage_dir(), &row, &Shutdown::detached(), None)
            .expect("rehash one");
        assert!(matches!(outcome, RehashOutcome::Match(_)));

        // Corrupt the stage payload: the catalog digest still names the old bytes.
        std::fs::write(world.stage_path(id_match), pattern(4 * 4096, 8)).expect("corrupt");
        let outcome = rehash_one(
            &mut io_buffer(), &world.config.paths.stage_dir(), &row, &Shutdown::detached(), None)
            .expect("rehash one");
        assert!(matches!(outcome, RehashOutcome::Mismatch(_)));
    }

    #[test]
    fn rehash_one_file_error_is_errored() {
        let world = ExtractTestWorld::new();
        let payload = pattern(4 * 4096, 9);
        let id = world.add_elected("m3.bin", &payload);
        std::fs::remove_file(world.stage_path(id)).expect("remove payload");
        let row = world.record(id);
        let outcome = rehash_one(
            &mut io_buffer(), &world.config.paths.stage_dir(), &row, &Shutdown::detached(), None)
            .expect("rehash one");
        assert!(matches!(outcome, RehashOutcome::Errored(_, FileStatError::Io { .. })));
    }

    #[test]
    fn rehash_one_force_abort_returns_none() {
        let world = ExtractTestWorld::new();
        let payload = pattern(16 * 1024 * 1024, 11);
        let id = world.add_elected("m4.bin", &payload);
        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let pb = world.progress.thread_bar(0);
        let sh = world.shutdown.clone();
        let pbo = pb.clone();
        let sh2 = sh.clone();
        let trigger = thread::spawn(move || {
            for _ in 0..20_000 {
                if pbo.position() > 0 {
                    sh2.request_force();
                    return;
                }
                thread::sleep(Duration::from_millis(1));
            }
            sh2.request_force();
        });
        // Drive with a live bar so the trigger can see progress.
        let outcome = {
            let row = world.record(id);
            rehash_one(&mut io_buffer(), &world.config.paths.stage_dir(), &row, &sh, Some(&pb))
        };
        trigger.join().expect("join trigger");
        world.progress.drop_thread_bars();
        assert!(outcome.is_none(), "force mid-file must discard the outcome");
    }

    #[test]
    fn rehash_worker_sends_outcome_then_none_on_channel_close() {
        let world = ExtractTestWorld::new();
        let payload = pattern(4 * 4096, 12);
        let id = world.add_elected("w1.bin", &payload);
        let row = world.record(id);
        let (work_s, work_r) = bounded::<StrippedRecord>(2);
        let (out_s, out_r) = bounded::<Option<RehashOutcome>>(8);

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh = world.shutdown.clone();
        let sd = world.config.paths.stage_dir().clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let worker = thread::Builder::new().name("rehash-worker-test".into())
            .spawn(move || rehash_worker(sd, bar, sh, wr, os))
            .expect("spawn rehash worker");
        drop(work_r);
        drop(out_s);
        work_s.send(row).expect("send row");
        drop(work_s);

        match out_r.recv().expect("recv outcome") {
            Some(o) => assert!(matches!(o, RehashOutcome::Match(_))),
            None => panic!("expected an outcome, got None"),
        }
        assert!(matches!(out_r.recv().expect("recv terminal"), None));
        let _ = worker.join();
        world.progress.drop_thread_bars();
    }

    #[test]
    fn rehash_worker_interrupt_breaks_and_sends_none() {
        let world = ExtractTestWorld::new();
        let payload = pattern(4 * 4096, 13);
        let id = world.add_elected("w2.bin", &payload);
        let row = world.record(id);
        let (work_s, work_r) = bounded::<StrippedRecord>(2);
        let (out_s, out_r) = bounded::<Option<RehashOutcome>>(8);
        world.shutdown.request_graceful();
        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh = world.shutdown.clone();
        let sd = world.config.paths.stage_dir().clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let worker = thread::Builder::new().name("rehash-worker-test".into())
            .spawn(move || rehash_worker(sd, bar, sh, wr, os))
            .expect("spawn rehash worker");
        drop(work_r);
        drop(out_s);
        work_s.send(row).expect("send row");
        drop(work_s);

        // The row is pulled but graceful is pre-set: no outcome, just None.
        assert!(matches!(out_r.recv().expect("recv terminal"), None));
        let _ = worker.join();
        world.progress.drop_thread_bars();
    }

    #[test]
    fn rehash_worker_force_midfile_breaks_and_sends_none() {
        let world = ExtractTestWorld::new();
        let payload = pattern(16 * 1024 * 1024, 14);
        let id = world.add_elected("w3.bin", &payload);
        let row = world.record(id);
        let (work_s, work_r) = bounded::<StrippedRecord>(2);
        let (out_s, out_r) = bounded::<Option<RehashOutcome>>(8);
        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh = world.shutdown.clone();
        let sd = world.config.paths.stage_dir().clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let bar_obs = bar.clone();
        let worker = thread::Builder::new().name("rehash-worker-test".into())
            .spawn(move || rehash_worker(sd, bar, sh, wr, os))
            .expect("spawn rehash worker");
        drop(work_r);
        drop(out_s);
        work_s.send(row).expect("send row");

        let sh2 = world.shutdown.clone();
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

        assert!(matches!(out_r.recv().expect("recv terminal"), None));
        trigger.join().expect("join trigger");
        let _ = worker.join();
        world.progress.drop_thread_bars();
    }

    #[test]
    fn loop_processes_batch() {
        let world = ExtractTestWorld::new();
        let mut ids = Vec::<FileId>::new();
        for i in 0..3 {
            let payload = pattern(512 * 1024 + i, i as u8);
            ids.push(world.add_elected(format!("l{i}.bin").as_str(), &payload));
        }
        world.prepare_for_rehash();

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let counts = run_loop(&world.rt(), world.progress.thread_bar(0), 2);
        world.progress.drop_thread_bars();

        assert_eq!((counts.matches, counts.mismatches, counts.errors), (3, 0, 0));
        for id in ids {
            assert_eq!(world.phase(id), FilePhase::Rehashed);
        }
    }

    #[test]
    fn loop_graceful_mid_feed_finishes_in_flight_and_resumes() {
        let world = ExtractTestWorld::new();
        let mut ids = Vec::<FileId>::new();
        for i in 0..8 {
            let payload = pattern(4 * 1024 * 1024 + i, i as u8);
            ids.push(world.add_elected(format!("g{i}.bin").as_str(), &payload));
        }
        world.prepare_for_rehash();

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh2 = world.shutdown.clone();
        let bar_obs = bar.clone();
        let trigger = thread::spawn(move || {
            // Fire while the first file is up: the worker sets its length right
            // before hashing, so `length() > 0` means a read is in flight. The
            // `position > 0 → 0` reset window is too narrow to poll reliably.
            for _ in 0..20_000 {
                if let Some(len) = bar_obs.length() {
                    if len > 0 {
                        sh2.request_graceful();
                        return;
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
            sh2.request_graceful();
        });

        let counts1 = run_loop(&world.rt(), bar, 2);
        trigger.join().expect("join trigger");
        world.progress.drop_thread_bars();

        let done1 = counts1.matches + counts1.mismatches + counts1.errors;
        assert!(done1 >= 1);
        assert!(done1 < 8);
        // interrupt/resume keeps the ordering table (drop happens on success only)
        assert!(world.queue_table_exists());

        let sh3 = Shutdown::detached();
        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let counts2 = run_loop(&world.rt_with(&sh3), world.progress.thread_bar(0), 2);
        world.progress.drop_thread_bars();

        // exactly-once: the resumed loop only finishes what the first left over
        let done2 = counts2.matches + counts2.mismatches + counts2.errors;
        assert_eq!(done1 + done2, 8);
        for id in ids {
            assert_eq!(world.phase(id), FilePhase::Rehashed);
            // digest was never lost / re-set by a second hash of the same bytes
            assert_eq!(world.digest(id), Some(sha1_of(&std::fs::read(world.stage_path(id)).expect("payload"))));
        }
    }

    #[test]
    fn loop_force_discards_in_flight_but_drains_delivered() {
        let world = ExtractTestWorld::new();
        // size-DESC queue order: the 64 MiB file is hash-stage first (delivered),
        // the 32 MiB second (in-flight when force lands); the two smalls are fed
        // but never processed.
        let s1 = pattern(64 * 1024 * 1024, 21);
        let s2 = pattern(32 * 1024 * 1024, 22);
        let s3 = pattern(2 * 1024 * 1024, 23);
        let s4 = pattern(2 * 1024 * 1024, 24);
        let id1 = world.add_elected("f1.bin", &s1);
        let id2 = world.add_elected("f2.bin", &s2);
        let id3 = world.add_elected("f3.bin", &s3);
        let id4 = world.add_elected("f4.bin", &s4);
        let s2_len = s2.len() as u64;
        world.prepare_for_rehash();

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh2 = world.shutdown.clone();
        let bar_obs = bar.clone();
        let trigger = thread::spawn(move || {
            // The worker is mid-`f2.bin` when its bar carries that file's length
            // and some position: force aborts the remainder.
            for _ in 0..20_000 {
                if let Some(len) = bar_obs.length() {
                    if len == s2_len && bar_obs.position() > 0 {
                        sh2.request_force();
                        return;
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
            sh2.request_force();
        });

        let counts = run_loop(&world.rt(), bar, 4);
        trigger.join().expect("join trigger");
        world.progress.drop_thread_bars();

        // Only the already-delivered 64 MiB outcome was committed (exact).
        assert_eq!((counts.matches, counts.mismatches, counts.errors), (1, 0, 0));
        assert_eq!(world.phase(id1), FilePhase::Rehashed);
        assert_eq!(world.phase(id2), FilePhase::ExtractFiltered);
        assert_eq!(world.phase(id3), FilePhase::ExtractFiltered);
        assert_eq!(world.phase(id4), FilePhase::ExtractFiltered);
        assert!(!world.flag(id2, FileFlag::ErrorWhileRehashing));
        assert!(!world.flag(id2, FileFlag::RehashMismatch));
        assert!(world.errors(id2).is_empty());
        assert!(world.queue_table_exists());

        // Resume with a fresh shutdown: everything still pending re-verifies.
        let sh3 = Shutdown::detached();
        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let counts2 = run_loop(&world.rt_with(&sh3), world.progress.thread_bar(0), 4);
        world.progress.drop_thread_bars();
        assert_eq!(counts.matches + counts2.matches, 4);
        for id in [id1, id2, id3, id4] {
            assert_eq!(world.phase(id), FilePhase::Rehashed);
        }
    }

    #[test]
    fn run_skip_rehash_promotes_everything() {
        let mut world = ExtractTestWorld::new();
        let payload = pattern(1024, 31);
        let id_a = world.add_elected("a.bin", &payload);
        let id_d = world.add_dup("d.bin", id_a);
        let id_n = world.add_non_file("n.bin");
        world.config.scan.rehash = false;

        run(&world.rt()).expect("run skip");

        for id in [id_a, id_d, id_n] {
            assert_eq!(world.phase(id), FilePhase::Rehashed);
        }
        assert!(!world.queue_table_exists(), "skip must not create the queue");
    }

    #[test]
    fn run_pending_zero_returns_ok() {
        let world = ExtractTestWorld::new();
        run(&world.rt()).expect("run empty");
        assert!(!world.queue_table_exists());
    }

    #[test]
    fn run_completes_and_drops_queue() {
        let world = ExtractTestWorld::new();
        let mut ids = Vec::<FileId>::new();
        for i in 0..3 {
            let payload = pattern(1024 * 1024 + i, i as u8);
            ids.push(world.add_elected(format!("r{i}.bin").as_str(), &payload));
        }
        run(&world.rt()).expect("run");

        for id in ids {
            assert_eq!(world.phase(id), FilePhase::Rehashed);
        }
        assert!(!world.queue_table_exists(), "queue dropped on success");
    }

    #[test]
    fn run_mismatch_endgame() {
        let mut world = ExtractTestWorld::new();
        world.config.force = false;
        let payload = pattern(1024 * 1024, 41);
        let id = world.add_elected("mm.bin", &payload);
        std::fs::write(world.stage_path(id), pattern(1024 * 1024, 42))
            .expect("corrupt payload");

        let res = run(&world.rt());
        assert!(matches!(&res, Err(Error::Config(msg)) if msg.contains("Corruption detected")));
        assert_eq!(world.phase(id), FilePhase::Rehashed);
        assert!(world.flag(id, FileFlag::RehashMismatch));

        // --force path: the phase completes and records the mismatch.
        world.config.force = true;
        let sh3 = Shutdown::detached();
        run(&world.rt_with(&sh3)).expect("force run");
        assert_eq!(world.phase(id), FilePhase::Rehashed);
        assert!(world.flag(id, FileFlag::RehashMismatch));
    }

    #[test]
    fn run_errors_fail_fast() {
        let mut world = ExtractTestWorld::new();
        let payload = pattern(1024 * 1024, 51);
        let id = world.add_elected("err.bin", &payload);
        std::fs::remove_file(world.stage_path(id)).expect("remove payload");

        world.config.process.fail_fast = true;
        let res = run(&world.rt());
        assert!(matches!(&res, Err(Error::Config(msg)) if msg.contains("Encountered 1 errors")));
        assert_eq!(world.phase(id), FilePhase::Rehashed);
        assert!(world.flag(id, FileFlag::ErrorWhileRehashing));
        let records = world.errors(id);
        assert_eq!(records.len(), 1);
        assert!(records[0].error_type.starts_with("Io/"));

        // Without fail-fast the phase completes and warns instead.
        let world2 = ExtractTestWorld::new();
        let payload2 = pattern(1024 * 1024, 52);
        let id2 = world2.add_elected("err2.bin", &payload2);
        std::fs::remove_file(world2.stage_path(id2)).expect("remove payload");
        run(&world2.rt()).expect("run tolerates errors");
        assert_eq!(world2.phase(id2), FilePhase::Rehashed);
        assert!(world2.flag(id2, FileFlag::ErrorWhileRehashing));
        assert_eq!(world2.errors(id2).len(), 1);
    }

    #[test]
    fn run_graceful_interrupt_tail_and_resume() {
        run_interrupt_tail(true);
    }

    #[test]
    fn run_force_interrupt_tail_and_resume() {
        run_interrupt_tail(false);
    }

    /// Interrupt before the phase starts (the repo convention for testing `run`'s
    /// interrupt tail — `run` materializes its worker bars internally, so a
    /// mid-run bar trigger is not reachable; mid-run interrupt *behavior* is
    /// pinned by the loop tests 15/16 above). Asserts `run` maps the interrupt to
    /// `Err(Interrupted)`, keeps the ordering table (drop is success-path only),
    /// then a fresh `run` with a detached shutdown completes the whole phase.
    fn run_interrupt_tail(graceful: bool) {
        let world = ExtractTestWorld::new();
        let mut ids = Vec::<FileId>::new();
        for i in 0..4 {
            let payload = pattern(16 * 1024 * 1024 + i, i as u8);
            ids.push(world.add_elected(format!("i{i}.bin").as_str(), &payload));
        }
        if graceful {
            world.shutdown.request_graceful();
        } else {
            world.shutdown.request_force();
        }

        let res = run(&world.rt());
        assert!(matches!(res, Err(Error::Interrupted)));
        assert!(world.queue_table_exists(), "queue survives the interrupt");

        let sh3 = Shutdown::detached();
        run(&world.rt_with(&sh3)).expect("resume run");
        for id in ids {
            assert_eq!(world.phase(id), FilePhase::Rehashed);
        }
    }
}