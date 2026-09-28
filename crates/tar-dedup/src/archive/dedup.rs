use std::path::Path;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};
use indicatif::ProgressBar;
use std::fs::File;
use std::io::Read;
use std::mem::take;

use crate::archive::ArchiveRTArgs;
use crate::cli::DedupMode;
use crate::common::files::warn_if_times_changed;
use crate::common::{at_least_one_running, io_buffer};
use crate::db::ErrorPhase;
use crate::db::dedup::{CompareOutcome, ComparePair, compare_pair};
use crate::db::flags::ErrorFlags;
use crate::db::types::{FileId, FilePhase, StrippedRecord};
use crate::db::{Database, Recorder};
use crate::error::{Error, FileStatError, Result};
use crate::progress::BarKind;
use crate::shutdown::Shutdown;

// Producer/consumer bounds (see plans/dedup-crossbeam.md). The input queue
// bounds in-flight pairs; `dedup_inflight` (a TEMP table) makes the candidate
// re-scan exactly-once so the feed can "start again" at any time without a
// global pool-exhausted barrier.
const WORK_CAPACITY: usize = 10_000;
const OUT_CAPACITY: usize = 20_000;
const FEED_CHUNK: usize = 1_024;      // rows pulled from list_pending_comparisons per round
const DRAIN_CHUNK: usize = 5_000;     // outcomes committed per transaction

// =================================================================================================
pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    let eager_filter = rt.config.filter.eager_filter;

    let catalog = rt.db.count_entries()?;
    // Early promote db entries we do not process in this phase.
    let ineligible = rt.db.promote_ineligible_entries_to_dedup(eager_filter)?;
    // The bulk skips above left the phase; credit the global for each of them.
    rt.progress.inc_global(ineligible);

    let prev_phase = if eager_filter {
        FilePhase::Hashed
    } else {
        FilePhase::Filtered
    };

    match rt.config.pipeline.dedup_mode {
        DedupMode::Regular => run_regular_fsm(rt, eager_filter, prev_phase, catalog, ineligible),
        DedupMode::Hash => run_hash_mode(rt, eager_filter, prev_phase, catalog),
        DedupMode::None => run_none_mode(rt, eager_filter, prev_phase, catalog),
    }
}

/// Regular dedup: the byte-compare FSM over `(sha1, size)` groups (workers +
/// `dedup_progress`/`dedup_inflight` temp tables).
fn run_regular_fsm(
    rt: &ArchiveRTArgs,
    eager_filter: bool,
    prev_phase: FilePhase,
    catalog: u64,
    ineligible: u64)
    -> Result<()> {
    // Singleton groups (unique content) need no compare round.
    let skipped_singleton = rt.db.promote_singleton_filtered_to_deduped(eager_filter)?;
    rt.progress.inc_global(skipped_singleton);

    // Group state machine tables (`dedup_inflight` is a per-connection TEMP
    // table, cleared with it; `dedup_progress` survives interrupts).
    rt.db.create_temp_dedup_table()?;
    rt.db.populate_temp_table(eager_filter)?;

    // The bar tracks the files inside duplicate `(sha1, size)` groups — both
    // the ones still to promote and those already `deduped` (resume anchor).
    let phase_total = rt.db.count_dedup_phase_total(eager_filter)?;
    let phase_done = rt.db.count_dedup_phase_position(eager_filter)?;
    rt.progress.set_phase_total(phase_total);
    rt.progress.set_phase_position(phase_done);

    tracing::info!(
        catalog,
        ineligible_files = ineligible,
        skipped_singleton,
        dedup_candidates = phase_total,
        already_deduped = phase_done,
        jobs = rt.config.process.io_jobs,
        "dedup pass"
    );

    if rt.db.count_pending_dedup_groups()? == 0 {
        sanity_check_flags(rt.db, prev_phase)?;
        return Ok(());
    }

    let jobs = rt.config.process.io_jobs;
    let detect_hardlinks = !rt.config.indexing.no_hardlink_detection;
    let mut thread_handles = Vec::with_capacity(jobs);

    // One bar per worker, materialized now so each worker can take its bar by
    // value; workers never touch `ProgressBarSet`.
    rt.progress.create_thread_bars(BarKind::Bytes, jobs);
    let mut bars = Vec::<ProgressBar>::new();
    for i in 0..jobs {
        bars.push(rt.progress.thread_bar(i));
    }

    let (work_s, work_r) = bounded::<ComparePair>(WORK_CAPACITY);
    let (out_s, out_r) = bounded::<Option<CompareOutcome>>(OUT_CAPACITY);

    for i in 0..jobs {
        let bar = bars[i].clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let sh = rt.shutdown.clone();
        thread_handles.push(thread::Builder::new()
            .name(format!("dedup-worker-{i}").into())
            .spawn(move || compare_worker(bar, sh, wr, os, detect_hardlinks))
            .expect("spawn dedup worker"));
    }
    drop(work_r);
    drop(out_s);

    let (fail_fast_hit, completed) = run_enqueue_dequeue_loop_dedup(
        &rt, work_s, out_r, thread_handles
    )?;

    rt.progress.drop_thread_bars();
    if fail_fast_hit {
        return Err(Error::Config(
            "dedup fail-fast: at least one group could not elect a canonical \
            (compare error(s) recorded)".to_string()
        ));
    }

    match rt.shutdown.is_interrupted() {
        true => {
            let msg = if rt.shutdown.is_force() {
                "dedup force-aborted; in-flight compares discarded"
            } else {
                "dedup stopped; completed compares saved"
            };
            tracing::warn!(saved = completed, "{msg}");
            Err(Error::Interrupted)
        }
        false => {
            sanity_check_flags(rt.db, prev_phase)?;
            rt.db.drop_temp_dedup_table()?;
            tracing::info!("dedup complete");
            Ok(())
        }
    }
}

/// Bulk dedup modes (`Hash`/`None`): no byte compare, no temp tables, no
/// workers. `catalog` is the pre-dedup entry count; `ineligible` promo already
/// ran. Routes to the mode's SQL and finishes the phase like the FSM tail.
fn run_bulk_mode(
    rt: &ArchiveRTArgs,
    eager_filter: bool,
    prev_phase: FilePhase,
    catalog: u64,
    promote: impl FnOnce(&Database, bool) -> Result<u64>)
    -> Result<()> {
    let total = rt.db.count_dedup_eligible_total(eager_filter)?;
    let done = rt.db.count_dedup_eligible_position(eager_filter)?;
    rt.progress.set_phase_total(total);
    rt.progress.set_phase_position(done);
    tracing::info!(
        catalog,
        dedup_candidates = total,
        already_deduped = done,
        "dedup pass"
    );

    if total == 0 || done == total {
        sanity_check_flags(rt.db, prev_phase)?;
        return Ok(());
    }

    let moved = promote(rt.db, eager_filter)?;
    rt.progress.inc_both(moved);
    sanity_check_flags(rt.db, prev_phase)?;
    tracing::info!("dedup complete");
    Ok(())
}

fn run_hash_mode(
    rt: &ArchiveRTArgs,
    eager_filter: bool,
    prev_phase: FilePhase,
    catalog: u64)
    -> Result<()> {
    run_bulk_mode(rt, eager_filter, prev_phase, catalog, |db, eager| {
        db.promote_hash_mode_to_dedup(eager)
    })
}

fn run_none_mode(
    rt: &ArchiveRTArgs,
    eager_filter: bool,
    prev_phase: FilePhase,
    catalog: u64)
    -> Result<()> {
    let detect_hardlinks = !rt.config.indexing.no_hardlink_detection;
    run_bulk_mode(rt, eager_filter, prev_phase, catalog, |db, eager| {
        db.promote_none_mode_to_dedup(eager, detect_hardlinks)
    })
}

/// Function performs the stepping of the loop. Deals with fetching data from the db,
/// enqueueing the data in the queue, retrieving the results from the queue and stepping the state
/// of the db.
fn run_enqueue_dequeue_loop_dedup(
    rt: &ArchiveRTArgs,
    send: Sender<ComparePair>,
    recv: Receiver<Option<CompareOutcome>>,
    mut handles: Vec<thread::JoinHandle<()>>)
    -> Result<(bool, u64)> {

    // Feed cursor over the candidate space. `dedup_inflight` (TEMP) marks pairs
    // handed out but not yet applied, so the scan can restart at id 0 any time
    // without re-comparing an in-flight pair.
    let eager_filter = rt.config.filter.eager_filter;
    let fail_fast = rt.config.process.fail_fast;
    let mut recorder = Recorder::new(rt.db, !rt.config.process.no_errors);

    // Feeder running variables
    let mut feed_buf = Vec::<(StrippedRecord, StrippedRecord)>::new();
    let mut feed_i = 0usize;

    // Loop runtime state
    let mut feed_total = 0u64;
    let mut exited_threads = 0u64;
    let mut dequeued_total = 0u64;
    let mut completed = 0u64;
    let mut fail_fast_hit = false;
    let mut pending_out = Vec::<CompareOutcome>::new();
    #[warn(unused_assignments)]
    let mut busy = false;

    let mut apply_chunk = |pending: &mut Vec<CompareOutcome>|
        -> Result<(u64, u64)> {
        let items = take(pending);
        if items.is_empty() {
            return Ok((0, 0));
        }
        let n = items.len() as u64;
        let resolved = rt.db.ingest_compare_outcome(&items)?;

        // Apply results
        for item in items.iter() {
            record_dedup_error(&mut recorder, &item);
        }
        Ok((n, resolved))
    };

    let mut drain_chunk = |
        is_busy: &mut  bool, exited: &mut u64, dequeue: &mut u64, override_drain: bool|
        -> Result<()> {
        // 1) Drain finished outcomes into a small batch.
        while pending_out.len() < DRAIN_CHUNK {
            match recv.try_recv() {
                Ok(Some(outcome)) => {
                    pending_out.push(outcome);
                    *is_busy = true;
                    *dequeue += 1;
                }
                Ok(None) => *exited += 1,
                Err(_) => break
            }
        }
        if pending_out.len() >= DRAIN_CHUNK || override_drain {
            let (n, resolved) = apply_chunk(&mut pending_out)?;
            completed += n;
            if resolved > 0 {
                rt.progress.inc_both(resolved);
            }
            *is_busy = true;
        }
        Ok(())
    };

    // Run the send receive loop. Each iteration drains the outcome channel
    // (override: a ragged tail applies immediately, so a small run's final
    // batch still advances the FSM in-loop), steps the per-group FSM, then
    // feeds the next candidate slice. The candidate scan restarts at id 0 on
    // every refill: `dedup_inflight` (TEMP) already makes the pull exactly-once
    // (see `list_pending_comparisons`), and a monotonic `last_candidate` cursor
    // would wrongly skip candidates re-elected across rounds.
    let mut feed_exhausted = false;
    loop {
        busy = false;
        if rt.shutdown.is_interrupted() {
            break;
        }
        // Handle dequeu side.
        drain_chunk(&mut busy, &mut exited_threads, &mut dequeued_total, true)?;

        // 2) Per-group FSM — advanced every iteration: guarded SQL flips only
        //    complete groups, so a slow 50 GiB group never stalls the rest.
        rt.db.searching_to_finished(eager_filter)?;
        let (errored, promoted) = rt.db.finish_to_error(eager_filter)?;
        if promoted > 0 {
            rt.progress.inc_both(promoted);
        }
        if fail_fast && errored > 0 {
            tracing::error!(
                errored_groups = errored,
                "dedup fail-fast: group(s) could not elect a canonical (compare error(s) recorded)"
            );
            fail_fast_hit = true;
            break;
        }
        let promoted1 = rt.db.finish_to_done(eager_filter)?;
        let promoted2 = rt.db.finish_to_ready(eager_filter)?;
        let (_, promoted3) = rt.db.ready_to_searching(eager_filter)?;
        if promoted1 + promoted2 + promoted3  > 0 {
            rt.progress.inc_both(promoted);
        }
        if rt.db.count_pending_dedup_groups()? == 0 {
            break;
        }
        // 3) Feed the pipeline. A fresh pull from the top on every refill lets
        //    freshly transitioned rounds (and their re-eligible candidates) be
        //    picked up; the empty-pull below only means every remaining
        //    candidate is in flight or none exists.
        if feed_i == feed_buf.len() {
            feed_buf = rt.db.list_pending_comparisons::<StrippedRecord>(
                eager_filter, 0, FEED_CHUNK as u64
            )?;
            feed_i = 0;
            feed_exhausted = feed_buf.is_empty();
        }
        if !feed_exhausted {
            let mut sent = Vec::<FileId>::new();
            while feed_i < feed_buf.len() {
                let candidate = &feed_buf[feed_i].0;
                let canonical = &feed_buf[feed_i].1;
                match send.try_send(compare_pair(canonical, candidate)) {
                    Ok(_) => {
                        sent.push(candidate.id);
                        busy = true;
                        feed_i += 1;
                        feed_total += 1;
                    }
                    Err(_) => break
                }
            }
            if !sent.is_empty() {
                rt.db.mark_inflight(&sent)?;
            }
        }
        // Nothing left to feed and every handed-out outcome is back in hand:
        // no further drain or FSM step can change anything. (The drain above
        // applies `pending_out` unconditionally, so it is empty here.) The
        // drain tail below still joins the workers and flushes the recorder.
        if feed_exhausted && feed_i == feed_buf.len()
            && dequeued_total == feed_total {
            break;
        }
        if !busy {
            thread::sleep(Duration::from_millis(1));
        }
    }
    // Cut the feed side; idle workers end their receive loop.
    drop(send);

    // Drain the remaining tasks scheduled to the workers are drained
    loop {
        busy = false;
        if rt.shutdown.is_interrupted()
            || dequeued_total == feed_total
            || exited_threads == rt.config.process.io_jobs as u64
            || !at_least_one_running(&handles.iter().collect()) {
            break;
        }
        drain_chunk(&mut busy, &mut exited_threads, &mut dequeued_total, true)?;
        if !busy {
            thread::sleep(Duration::from_millis(1));
        }
    }

    // Close out the worker threads before dropping the channel: each worker
    // sends its last outcome/None *before* it returns, so joining every handle
    // guarantees no `out.send` can hit a dropped receiver (panics on "result
    // channel closed"). `join` also honours "finish in-flight" on a graceful
    // stop (workers exit after their current pair resolves).
    for handle in take(&mut handles) {
        let _ = handle.join();
    }

    // Definitively drain whatever the workers still produce (incl. after an
    // interrupt: workers finish their in-flight pair, then the channel drops).
    drain_chunk(&mut busy, &mut exited_threads, &mut dequeued_total, true)?;
    recorder.flush()?;
    drop(recv);

    Ok((fail_fast_hit, completed))
}

/// Compare worker: pulls one pair at a time, byte-compares it, forwards the
/// outcome. Owns its bar and the two reusable read buffers; only touches the
/// channels, `tracing`, and its own `Shutdown` clone.
/// On a force abort the in-flight pair produces **no outcome** (the read aborts
/// via `check_in_flight`), so it stays pending for the resumed run.
fn compare_worker(
    bar: ProgressBar,
    shutdown: Shutdown,
    work: Receiver<ComparePair>,
    out: Sender<Option<CompareOutcome>>,
    detect_hardlinks: bool) {
    let mut buf_a = io_buffer();
    let mut buf_b = io_buffer();
    loop {
        match work.recv() {
            Ok(pair) => {
                if shutdown.check_between_files().is_err() {
                    break;
                }
                bar.reset();
                bar.set_length(pair.candidate_size);
                bar.set_message(format!("Comparing {}", pair.candidate_path.display()));
                warn_compare_pair_times(&pair);
                match compare_one(&pair, &shutdown, &mut buf_a, &mut buf_b, detect_hardlinks) {
                    Ok(outcome) => {
                        out.send(Some(outcome)).expect("dedup worker: result channel closed");
                    }
                    Err(Error::Interrupted) => break,
                    Err(e) => panic!(
                        "dedup compare error (should be Interrupted or outcome): {e}"
                    ),
                }
            }
            Err(_) => break
        }
    }
    out.send(None).expect("dedup worker: result channel closed");
}

/// Rerun the count_check_with_canonical_completed and return an error if the count is not 0.
fn sanity_check_flags(db: &Database, prev_phase: FilePhase) -> Result<()> {
    let n = db.count_check_with_canonical_completed()?;
    if n != 0 {
        panic!("dedup sanity check failed: {n} file(s) still have CheckWithCanonicalCompleted set");
    };
    let leftover = db.count_files_in_phase(prev_phase)?;
    if leftover != 0 {
        panic!(
            "dedup finished with {leftover} file(s) still in {} \
            (expected 0 after skips + rounds)",
            prev_phase.as_str()
        );
    }
    Ok(())
}

/// Run when a worker pulls a pair — just before compare, not in bulk upfront.
fn warn_compare_pair_times(pair: &ComparePair) -> (bool, bool) {
    let canonical = warn_if_times_changed(
        &pair.canonical_path,
        pair.canonical_mtime,
        pair.canonical_atime,
        pair.canonical_ctime,
    );
    let candidate = warn_if_times_changed(
        &pair.candidate_path,
        pair.candidate_mtime,
        pair.candidate_atime,
        pair.candidate_ctime,
    );
    (canonical, candidate)
}

/// Reflect the outcome of a hardlink shortcut or byte-compare minus the
/// interrupt "no outcome" path: on [`Error::Interrupted`] the caller drops the
/// pair (it stays pending for resume) instead of turning it into an outcome.
fn compare_one(
    pair: &ComparePair,
    shutdown: &Shutdown,
    buf_a: &mut Vec<u8>,
    buf_b: &mut Vec<u8>,
    detect_hardlinks: bool)
    -> Result<CompareOutcome> {

    shutdown.check_between_files()?;

    let (cano, cand) = warn_compare_pair_times(pair);

    let pre_flight_check = if detect_hardlinks {
        match (pair.canonical_inode_id,
               pair.canonical_device_id,
               pair.candidate_inode_id,
               pair.candidate_device_id) {
            (Some(oi), Some(od), Some(ai), Some(ad))
            if oi == ai && od == ad => true,
            _ => false,
        }
    } else {
        false
    };
    // Interrupt must not become a CompareOutcome.
    let equal = match pre_flight_check {
        false => match files_equal(
            &pair.canonical_path, &pair.candidate_path, shutdown, buf_a, buf_b) {
            Ok(v) => Ok(v),
            Err(Error::Interrupted) => return Err(Error::Interrupted),
            Err(e @ Error::FileStat(_)) => Err(compare_error_file_id(pair, &e)),
            Err(e) => panic!("unexpected compare error (not FileStat/Interrupted): {e}"),
        },
        true => Ok(true),
    };
    Ok(CompareOutcome {
        canonical_id: pair.canonical_id,
        candidate_id: pair.candidate_id,
        equal,
        canonical_modified: cano,
        candidate_modified: cand,
    })
}


/// Downcast a compare `Error` into the failing side's `(file_id, FileStatError)`.
/// `files_equal` only ever produces `FileStat` errors wrapping `Io` (or
/// `Interrupted`, handled above), so the path is reliable; anything else is
/// treated as a panic predicate.
/// PRECONDITION: The Function only covers the FileStat variant. Anything else results in a panic.
fn compare_error_file_id(pair: &ComparePair, e: &Error) -> (FileId, FileStatError) {
    let path = e.io_path().expect(
        "compare produced a non-Io, non-Interrupted error; \
         files_equal only ever returns Io or Interrupted");
    let file_id = if path == pair.canonical_path {
        pair.canonical_id
    } else if path == pair.candidate_path {
        pair.candidate_id
    } else {
        panic!(
            "compare IO error path is neither canonical nor candidate: \
             path={} canonical={} candidate={}",
            path.display(),
            pair.canonical_path.display(),
            pair.candidate_path.display(),
        );
    };
    (file_id, e.to_only_file_stat()
        .expect("PRECONDITION FAILED. FileStatError only expected in this function.")
    )
}

/// Record a per-file hash failure in the persistent error log (best-effort).
/// `hash_file` failures are `FileStat` (per-file) errors; the carried
/// [`FileStatError`](FileStatError) is recreated on the way in.
fn record_dedup_error(recorder: &mut Recorder, e: &CompareOutcome) {
    let (fid, e) = match &e.equal {
        Err((file_id, error)) => (file_id, error),
        Ok(_) => return,
    };
    recorder.record_file(
        *fid,
        ErrorPhase::Pipeline(crate::config::PipelinePhase::Dedup),
        e.recreate(),
        ErrorFlags::default(),
    );
}


/// Check that two files are binary identical (and have same length).
/// Returns our custom Error with io variant.
/// `buf_a`/`buf_b` are the caller's reusable read buffers (sized once), so the
/// 2×4 MiB-per-pair allocation of the old per-call buffers is avoided.
fn files_equal(
    a: &Path,
    b: &Path,
    shutdown: &Shutdown,
    buf_a: &mut Vec<u8>,
    buf_b: &mut Vec<u8>,
) -> Result<bool> {
    let mut fa = File::open(a).map_err(|e| Error::io(a, e))?;
    let mut fb = File::open(b).map_err(|e| Error::io(b, e))?;

    let len_a = fa.metadata().map_err(|e| Error::io(a, e))?.len();
    let len_b = fb.metadata().map_err(|e| Error::io(b, e))?.len();
    if len_a != len_b {
        return Ok(false);
    }

    loop {
        shutdown.check_in_flight()?;
        let na = fa.read(buf_a).map_err(|e| Error::io(a, e))?;
        let nb = fb.read(buf_b).map_err(|e| Error::io(b, e))?;
        if na == 0 && nb == 0 {
            return Ok(true);
        }
        if na != nb || buf_a[..na] != buf_b[..nb] {
            return Ok(false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::files::original_extension;
    use crate::common::start::StartPolicy;
    use crate::config::{
        ArchiveConfig, ArchivePipelineOptions, CaptureOptions, CleanupSettings, CompressionFormat,
        CompressionSettings, FilterOptions, IndexingOptions, InputOptions, OwnerPolicy, PathLayout,
        ProcessOptions, SparseOptions,
    };
    use crate::db::flags::{FileFlag, set_file_flag};
    use crate::db::types::{FileId, FileType, NewFileRecord};
    use crate::db::{Database, ErrorPhase, Recorder};
    use crate::progress::{ARCHIVE_MULTIPLIER, ProgressBarSet};
    use chrono::{DateTime, Utc};
    use nix::unistd::geteuid;
    use rusqlite::named_params;
    use std::os::unix::fs::PermissionsExt;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::{Path, PathBuf};

    fn pattern(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| ((i % 251) as u8) ^ seed).collect()
    }

    fn buffers() -> (Vec<u8>, Vec<u8>) {
        (io_buffer(), io_buffer())
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
                io_jobs: 2,
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
            // The internal include rule the filter phase installs: without it
            // `apply_no_filter` (-1) violates the FK.
            db.with_transaction(|conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO filter_reason_archive (id, source, line, expression) \
                     VALUES (-1, 'internal', NULL, '.*')", [],
                ).expect("seed internal include rule");
                Ok(())
            }).expect("seed internal include rule tx");
            Self {
                dir,
                db,
                shutdown: Shutdown::detached(),
                progress: ProgressBarSet::new(ARCHIVE_MULTIPLIER),
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
            self.insert_recorded(&self.path(name), payload, None, None, None)
        }

        fn add_file_recorded(
            &self, name: &str, payload: &[u8],
            mtime: Option<DateTime<Utc>>, atime: Option<DateTime<Utc>>, ctime: Option<DateTime<Utc>>)
            -> FileId {
            self.insert_recorded(&self.path(name), payload, mtime, atime, ctime)
        }

        fn insert_recorded(
            &self, path: &Path, payload: &[u8],
            mtime: Option<DateTime<Utc>>, atime: Option<DateTime<Utc>>, ctime: Option<DateTime<Utc>>)
            -> FileId {
            std::fs::write(path, payload).expect("write test payload");
            self.db.insert_file(&NewFileRecord {
                abs_path: PathBuf::from(path),
                ext: original_extension(path),
                size: payload.len() as u64,
                mtime, atime, ctime,
                uid: None, gid: None, mode: None,
                ftype: Some(FileType::File),
                xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None, link_dst: None,
                device_id: None, inode_id: None, major: None, minor: None,
            }).expect("insert test file");
            self.db.file_id_by_abs_path(path).expect("lookup test file").expect("file present")
        }

        fn seed_sha1(&self, id: FileId, sha1_byte: u8) {
            self.db.with_transaction(|conn| {
                let n = conn.execute(
                    "UPDATE files SET sha1 = :sha1 WHERE id = :id",
                    named_params! {
                        ":sha1": [sha1_byte; 20].as_slice(),
                        ":id": id.0,
                    },
                ).expect("seed sha1");
                assert_eq!(n, 1);
                Ok(())
            }).expect("seed sha1 tx");
        }

        fn seed_eager(&self) {
            self.db.with_transaction(|conn| {
                conn.execute(
                    "UPDATE files SET phase = 'hashed'", [],
                ).expect("seed eager phase");
                Ok(())
            }).expect("seed eager tx");
        }

        fn phase(&self, id: FileId) -> FilePhase {
            self.db.get_file_by_id::<StrippedRecord>(id)
                .expect("get row")
                .expect("row present")
                .phase
        }

        fn canonical_of(&self, id: FileId) -> Option<FileId> {
            self.db.get_file_by_id::<StrippedRecord>(id)
                .expect("get row")
                .expect("row present")
                .canonical_id
        }

        fn flag(&self, id: FileId, flag: FileFlag) -> bool {
            self.db.get_file_flag(id, flag).expect("get flag")
        }
    }

    fn pair_for(world: &TestWorld, canonical_id: FileId, candidate_id: FileId) -> ComparePair {
        let canonical = world.db.get_file_by_id::<StrippedRecord>(canonical_id)
            .expect("canonical row").expect("canonical present");
        let candidate = world.db.get_file_by_id::<StrippedRecord>(candidate_id)
            .expect("candidate row").expect("candidate present");
        compare_pair(&canonical, &candidate)
    }

    fn seed_rows(world: &TestWorld, specs: Vec<(FileId, u8)>) {
        for (id, byte) in specs {
            world.seed_sha1(id, byte);
        }
    }

    fn assert_equal_outcome(outcome: &CompareOutcome, expected: bool) {
        match outcome.equal {
            Ok(v) => assert_eq!(v, expected),
            Err(_) => panic!("expected equal outcome, got a compare error"),
        }
    }

    #[test]
    fn files_equal_same_content() {
        let world = TestWorld::new();
        let a = world.path("a.bin");
        let b = world.path("b.bin");
        let payload = pattern(1024 * 1024, 3);
        std::fs::write(&a, &payload).expect("write a");
        std::fs::write(&b, &payload).expect("write b");
        let (mut ba, mut bb) = buffers();
        assert_eq!(
            files_equal(&a, &b, &world.shutdown, &mut ba, &mut bb).expect("compare"),
            true
        );
    }

    #[test]
    fn files_equal_diff_length() {
        let world = TestWorld::new();
        let a = world.path("a.bin");
        let b = world.path("b.bin");
        std::fs::write(&a, &pattern(1024 * 1024, 3)).expect("write a");
        std::fs::write(&b, &pattern(1024 * 1024 + 1, 3)).expect("write b");
        let (mut ba, mut bb) = buffers();
        assert_eq!(
            files_equal(&a, &b, &world.shutdown, &mut ba, &mut bb).expect("compare"),
            false
        );
    }

    #[test]
    fn files_equal_diff_content_same_length() {
        let world = TestWorld::new();
        let a = world.path("a.bin");
        let b = world.path("b.bin");
        std::fs::write(&a, &pattern(1024 * 1024, 3)).expect("write a");
        std::fs::write(&b, &pattern(1024 * 1024, 4)).expect("write b");
        let (mut ba, mut bb) = buffers();
        assert_eq!(
            files_equal(&a, &b, &world.shutdown, &mut ba, &mut bb).expect("compare"),
            false
        );
    }

    #[test]
    fn files_equal_force_pre_set_interrupts() {
        let world = TestWorld::new();
        let a = world.path("a.bin");
        let b = world.path("b.bin");
        let payload = pattern(1024 * 1024, 3);
        std::fs::write(&a, &payload).expect("write a");
        std::fs::write(&b, &payload).expect("write b");
        world.shutdown.request_force();
        let (mut ba, mut bb) = buffers();
        let res = files_equal(&a, &b, &world.shutdown, &mut ba, &mut bb);
        assert!(matches!(res, Err(Error::Interrupted)));
    }

    #[test]
    fn files_equal_open_permission_denied() {
        if geteuid().is_root() {
            return;
        }
        let world = TestWorld::new();
        let a = world.path("a.bin");
        let b = world.path("b.bin");
        std::fs::write(&a, &pattern(64 * 1024, 3)).expect("write a");
        std::fs::write(&b, &pattern(64 * 1024, 3)).expect("write b");
        std::fs::set_permissions(&b, std::fs::Permissions::from_mode(0)).expect("chmod 000");
        let (mut ba, mut bb) = buffers();
        let res = files_equal(&a, &b, &world.shutdown, &mut ba, &mut bb);
        assert!(matches!(res, Err(Error::FileStat(_))));
        if let Err(Error::FileStat(fse)) = res {
            assert_eq!(fse.io_path(), Some(b.to_path_buf()));
        }
    }

    #[test]
    fn compare_error_file_id_canonical_side() {
        let world = TestWorld::new();
        let id_a = world.add_file("a.bin", &pattern(1024, 1));
        let id_b = world.add_file("b.bin", &pattern(1024, 2));
        let pair = pair_for(&world, id_a, id_b);
        let e = Error::FileStat(FileStatError::Io {
            path: pair.canonical_path.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied, "nope".to_string()),
        });

        let (fid, fse) = compare_error_file_id(&pair, &e);

        assert_eq!(fid, id_a);
        assert_eq!(fse.io_path(), Some(pair.canonical_path.clone()));
    }

    #[test]
    fn compare_error_file_id_candidate_side() {
        let world = TestWorld::new();
        let id_a = world.add_file("a.bin", &pattern(1024, 1));
        let id_b = world.add_file("b.bin", &pattern(1024, 2));
        let pair = pair_for(&world, id_a, id_b);
        let e = Error::FileStat(FileStatError::Io {
            path: pair.candidate_path.clone(),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied, "nope".to_string()),
        });

        let (fid, fse) = compare_error_file_id(&pair, &e);

        assert_eq!(fid, id_b);
        assert_eq!(fse.io_path(), Some(pair.candidate_path.clone()));
    }

    #[test]
    fn compare_error_file_id_unknown_path_panics() {
        let world = TestWorld::new();
        let id_a = world.add_file("a.bin", &pattern(1024, 1));
        let id_b = world.add_file("b.bin", &pattern(1024, 2));
        let pair = pair_for(&world, id_a, id_b);
        let e = Error::FileStat(FileStatError::Io {
            path: PathBuf::from("/tmp/other.bin"),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied, "nope".to_string()),
        });

        let res = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let _ = compare_error_file_id(&pair, &e);
            Ok(())
        }));
        assert!(res.is_err());
    }

    #[test]
    fn record_dedup_error_only_records_errors() {
        let world = TestWorld::new();
        let id = world.add_file("e.bin", &pattern(1024, 1));
        let mut recorder = Recorder::new(&world.db, true);
        record_dedup_error(&mut recorder, &CompareOutcome {
            canonical_id: FileId(1), candidate_id: id, equal: Ok(true),
            canonical_modified: false, candidate_modified: false,
        });
        record_dedup_error(&mut recorder, &CompareOutcome {
            canonical_id: FileId(1), candidate_id: id, equal: Ok(false),
            canonical_modified: false, candidate_modified: false,
        });
        record_dedup_error(&mut recorder, &CompareOutcome {
            canonical_id: FileId(1),
            candidate_id: id,
            equal: Err((id, FileStatError::Io {
                path: PathBuf::from("/tmp/e.bin"),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied, "denied".to_string()),
            })),
            canonical_modified: false,
            candidate_modified: false,
        });
        recorder.flush().expect("flush recorder");

        let records = world.db.get_records_by_file_id(id).expect("records");
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].file_id, Some(id));
        assert_eq!(records[0].error_type, "Io/PermissionDenied");
        assert_eq!(records[0].phase, ErrorPhase::Pipeline(crate::config::PipelinePhase::Dedup));
    }

    fn compare_one_pair(
        world: &TestWorld, canonical_id: FileId, candidate_id: FileId, detect: bool)
        -> CompareOutcome {
        let pair = pair_for(world, canonical_id, candidate_id);
        let (mut ba, mut bb) = buffers();
        compare_one(&pair, &world.shutdown, &mut ba, &mut bb, detect)
            .expect("compare_one")
    }

    #[test]
    fn compare_one_byte_compare_equal() {
        let world = TestWorld::new();
        let payload = pattern(1024 * 1024, 3);
        let id_a = world.add_file("a.bin", &payload);
        let id_b = world.add_file("b.bin", &payload);
        let outcome = compare_one_pair(&world, id_a, id_b, false);
        assert_equal_outcome(&outcome, true);
        assert_eq!(outcome.canonical_modified, false);
        assert_eq!(outcome.candidate_modified, false);
    }

    #[test]
    fn compare_one_byte_compare_unequal() {
        let world = TestWorld::new();
        let id_a = world.add_file("a.bin", &pattern(1024 * 1024, 3));
        let id_b = world.add_file("b.bin", &pattern(1024 * 1024, 4));
        let outcome = compare_one_pair(&world, id_a, id_b, false);
        assert_equal_outcome(&outcome, false);
    }

    #[test]
    fn compare_one_hardlink_preflight_short_circuits() {
        let world = TestWorld::new();
        // Two *different-content* paths sharing a stored (dev, inode): the
        // preflight must short-circuit the byte compare, so it still yields
        // equal without reading either payload.
        let id_l = world.add_file("hl_src.bin", &pattern(1024, 7));
        let id_q = world.add_file("hl_dst.bin", &pattern(1024, 8));
        // Also exercise a real hard-link pair (same inode by construction).
        std::fs::hard_link(&world.path("hl_src.bin"), &world.path("hl_real.bin"))
            .expect("hard link");
        world.db.with_transaction(|conn| {
            conn.execute(
                "UPDATE files SET dev = 7, inode = 99 WHERE id IN (:a, :b)",
                named_params! { ":a": id_l.0, ":b": id_q.0 },
            ).expect("seed dev/inode");
            Ok(())
        }).expect("seed dev/inode tx");
        let pair = pair_for(&world, id_l, id_q);
        let (mut ba, mut bb) = buffers();
        let outcome = compare_one(&pair, &world.shutdown, &mut ba, &mut bb, true)
            .expect("compare_one");
        assert_equal_outcome(&outcome, true);
        assert_eq!(outcome.candidate_id, id_q);
        // and the disabled-detection path still byte-compares (differs).
        let pair2 = pair_for(&world, id_l, id_q);
        let outcome2 = compare_one(&pair2, &world.shutdown, &mut ba, &mut bb, false)
            .expect("compare_one");
        assert_equal_outcome(&outcome2, false);
    }

    #[test]
    fn compare_one_times_modified_detected() {
        let world = TestWorld::new();
        let now = Utc::now();
        let stale = DateTime::from_timestamp(now.timestamp() - 3600, 0).expect("stale");
        let payload = pattern(128 * 1024, 4);
        let id_canon = world.add_file("c.bin", &payload);
        let id_cand = world.add_file_recorded("d.bin", &payload, Some(stale), None, None);

        let outcome = compare_one_pair(&world, id_canon, id_cand, false);

        assert_eq!(outcome.canonical_modified, false);
        assert_eq!(outcome.candidate_modified, true);
    }

    #[test]
    fn compare_one_graceful_pre_set_interrupts() {
        let world = TestWorld::new();
        let payload = pattern(1024 * 1024, 3);
        let id_a = world.add_file("a.bin", &payload);
        let id_b = world.add_file("b.bin", &payload);
        let pair = pair_for(&world, id_a, id_b);
        world.shutdown.request_graceful();
        let (mut ba, mut bb) = buffers();
        assert!(matches!(
            compare_one(&pair, &world.shutdown, &mut ba, &mut bb, false),
            Err(Error::Interrupted)
        ));
    }

    #[test]
    fn compare_one_force_pre_set_interrupts() {
        let world = TestWorld::new();
        let payload = pattern(1024 * 1024, 3);
        let id_a = world.add_file("a.bin", &payload);
        let id_b = world.add_file("b.bin", &payload);
        let pair = pair_for(&world, id_a, id_b);
        world.shutdown.request_force();
        let (mut ba, mut bb) = buffers();
        assert!(matches!(
            compare_one(&pair, &world.shutdown, &mut ba, &mut bb, false),
            Err(Error::Interrupted)
        ));
    }

    #[test]
    fn compare_one_io_error_mapped_to_file_id() {
        if geteuid().is_root() {
            return;
        }
        let world = TestWorld::new();
        let id_canon = world.add_file("a.bin", &pattern(64 * 1024, 3));
        let id_cand = world.add_file("b.bin", &pattern(64 * 1024, 3));
        std::fs::set_permissions(&world.path("b.bin"), std::fs::Permissions::from_mode(0))
            .expect("chmod 000");
        let pair = pair_for(&world, id_canon, id_cand);
        let (mut ba, mut bb) = buffers();
        let outcome = compare_one(&pair, &world.shutdown, &mut ba, &mut bb, false)
            .expect("compare_one");
        match outcome.equal {
            Err((fid, fse)) => {
                assert_eq!(fid, id_cand);
                assert_eq!(fse.io_path(), Some(pair.candidate_path.clone()));
            }
            Ok(_) => panic!("expected an IO error outcome for the unreadable candidate"),
        }
    }

    #[test]
    fn compare_pair_maps_records() {
        let world = TestWorld::new();
        let payload = pattern(4096, 3);
        let id_a = world.add_file("a.bin", &payload);
        let id_b = world.add_file("b.bin", &payload);
        let canon = world.db.get_file_by_id::<StrippedRecord>(id_a)
            .expect("canonical row").expect("present");
        let cand = world.db.get_file_by_id::<StrippedRecord>(id_b)
            .expect("candidate row").expect("present");

        let pair = compare_pair(&canon, &cand);

        assert_eq!(pair.canonical_id, id_a);
        assert_eq!(pair.candidate_id, id_b);
        assert_eq!(pair.canonical_path, canon.abs_path);
        assert_eq!(pair.candidate_path, cand.abs_path);
        assert_eq!(pair.candidate_size, cand.size);
        assert_eq!(pair.canonical_inode_id, canon.inode_id);
        assert_eq!(pair.candidate_device_id, cand.device_id);
        assert_eq!(pair.canonical_mtime, canon.mtime);
        assert_eq!(pair.candidate_atime, cand.atime);
        assert_eq!(pair.canonical_ctime, canon.ctime);
    }

    #[test]
    fn compare_worker_sends_none_on_graceful_exit() {
        let world = TestWorld::new();
        let id_a = world.add_file("a.bin", &pattern(4096, 1));
        let id_b = world.add_file("b.bin", &pattern(4096, 2));
        let pair = pair_for(&world, id_a, id_b);
        let (work_s, work_r) = bounded::<ComparePair>(4);
        let (out_s, out_r) = bounded::<Option<CompareOutcome>>(8);
        work_s.send(pair).expect("send pair");
        world.shutdown.request_graceful();
        drop(work_s);

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh = world.shutdown.clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let worker = thread::Builder::new().name("dedup-worker-test".into())
            .spawn(move || compare_worker(bar, sh, wr, os, false))
            .expect("spawn dedup worker");
        drop(work_r);
        drop(out_s);

        // The pair is pulled but graceful is already set: no outcome, just None.
        match out_r.recv().expect("recv terminal") {
            None => (),
            Some(_) => panic!("no outcome expected on graceful pre-set"),
        }
        let _ = worker.join();
        world.progress.drop_thread_bars();
    }

    #[test]
    fn compare_worker_sends_none_after_force_mid_compare() {
        let world = TestWorld::new();
        let payload = pattern(16 * 1024 * 1024, 3);
        let id_a = world.add_file("a.bin", &payload);
        let id_b = world.add_file("b.bin", &payload);
        let pair = pair_for(&world, id_a, id_b);
        let (work_s, work_r) = bounded::<ComparePair>(2);
        let (out_s, out_r) = bounded::<Option<CompareOutcome>>(8);

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh = world.shutdown.clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let bar_obs = bar.clone();
        let worker = thread::Builder::new().name("dedup-worker-test".into())
            .spawn(move || compare_worker(bar, sh, wr, os, false))
            .expect("spawn dedup worker");
        drop(work_r);
        drop(out_s);

        work_s.send(pair).expect("send pair");
        let sh2 = world.shutdown.clone();
        let trigger = thread::spawn(move || {
            for _ in 0..20_000 {
                if let Some(len) = bar_obs.length() {
                    if len > 0 {
                        sh2.request_force();
                        return;
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
            sh2.request_force();
        });
        drop(work_s);

        // The in-flight compare aborts: no Some for the pair, the worker exits
        // with None (I3: the drain must not drop the channel first).
        match out_r.recv().expect("recv terminal") {
            None => (),
            Some(_) => panic!("force-aborted compare must not produce an outcome"),
        }
        trigger.join().expect("join trigger");
        let _ = worker.join();
        world.progress.drop_thread_bars();
    }

    /// One worker + one bar, driving `run_enqueue_dequeue_loop_dedup` directly.
    fn run_loop(
        world: &TestWorld, bar: ProgressBar, work_cap: usize) -> (bool, u64) {
        let (work_s, work_r) = bounded::<ComparePair>(work_cap);
        let (out_s, out_r) = bounded::<Option<CompareOutcome>>(OUT_CAPACITY);
        let detect = !world.config.indexing.no_hardlink_detection;
        let sh = world.shutdown.clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let mut handles = Vec::<thread::JoinHandle<()>>::new();
        let worker = thread::Builder::new().name("dedup-worker-test".into())
            .spawn(move || compare_worker(bar, sh, wr, os, detect))
            .expect("spawn dedup worker");
        handles.push(worker);
        drop(work_r);
        drop(out_s);
        let rt = world.rt();
        run_enqueue_dequeue_loop_dedup(&rt, work_s, out_r, handles)
            .expect("dedup send/receive loop")
    }

    /// The `run()` preamble minus the bar pool and worker spawn: promotions,
    /// temp group tables + populate. Lets loop-level tests reuse `run`'s state.
    fn prepare_for_loop(world: &TestWorld) {
        let eager = world.config.filter.eager_filter;
        world.db.promote_ineligible_entries_to_dedup(eager).expect("ineligible");
        world.db.promote_singleton_filtered_to_deduped(eager).expect("singleton");
        world.db.create_temp_dedup_table().expect("create temp");
        world.db.populate_temp_table(eager).expect("populate temp");
    }

    #[test]
    fn loop_exit_via_dequeued_eq_feed_total() {
        let world = TestWorld::new();
        let payload = pattern(512 * 1024, 3);
        let id_canon = world.add_file("c.bin", &payload);
        let id_cand = world.add_file("d.bin", &payload);
        world.db.apply_no_filter_archive().expect("filter");
        seed_rows(&world, Vec::from([(id_canon, 7), (id_cand, 7)]));
        prepare_for_loop(&world);

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let (fail_fast, completed) = run_loop(&world, world.progress.thread_bar(0), 2);
        world.progress.drop_thread_bars();

        assert_eq!(fail_fast, false);
        assert_eq!(completed, 1);
        assert_eq!(world.canonical_of(id_cand), Some(id_canon));
        assert_eq!(world.phase(id_cand), FilePhase::Deduped);
    }

    #[test]
    fn loop_exit_via_exited_threads_and_one_running() {
        let world = TestWorld::new();
        let id_canon = world.add_file("c.bin", &pattern(512 * 1024, 3));
        let id_cand = world.add_file("d.bin", &pattern(512 * 1024, 3));
        world.db.apply_no_filter_archive().expect("filter");
        seed_rows(&world, Vec::from([(id_canon, 7), (id_cand, 7)]));
        world.shutdown.request_graceful();

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let (fail_fast, completed) = run_loop(&world, world.progress.thread_bar(0), 2);
        world.progress.drop_thread_bars();

        // Graceful pre-set: the loop breaks immediately, the worker exits with
        // None, and the drain tail still terminates via exited/joint handles.
        assert_eq!(fail_fast, false);
        assert_eq!(completed, 0);
    }

    #[test]
    fn run_nothing_to_do_exits_early() {
        let world = TestWorld::new();
        let id_a = world.add_file("a.bin", &pattern(1024 * 1024, 1));
        let id_b = world.add_file("b.bin", &pattern(1024 * 1024, 2));
        world.db.apply_no_filter_archive().expect("filter");
        seed_rows(&world, Vec::from([(id_a, 3), (id_b, 4)]));

        run(&world.rt()).expect("run");

        assert_eq!(world.phase(id_a), FilePhase::Deduped);
        assert_eq!(world.phase(id_b), FilePhase::Deduped);
        assert_eq!(
            world.db.count_pending_dedup_groups().expect("pending"),
            0
        );
    }

    fn seed_dedup_world(payload: &[u8], alt: u8) -> (TestWorld, Vec<(FileId, u8)>) {
        let world = TestWorld::new();
        let e1 = world.add_file("e1.bin", payload);
        let e2 = world.add_file("e2.bin", payload);
        let e3 = world.add_file("e3.bin", payload);
        let u1 = world.add_file("u1.bin", &pattern(payload.len(), alt));
        let u2 = world.add_file("u2.bin", &pattern(payload.len(), alt + 1));
        world.db.apply_no_filter_archive().expect("filter");
        seed_rows(&world, Vec::from([
            (e1, 7), (e2, 7), (e3, 7),
            (u1, 8), (u2, 8),
        ]));
        (world, Vec::from([(e1, 7), (e2, 7), (e3, 7), (u1, 8), (u2, 8)]))
    }

    #[test]
    fn run_basic_equal_and_unequal_groups() {
        let payload = pattern(512 * 1024, 5);
        let (world, specs) = seed_dedup_world(&payload, 9);
        let ids = specs.into_iter().map(|(id, _)| id).collect::<Vec<FileId>>();
        let (e1, e2, e3, u1, u2) = (ids[0], ids[1], ids[2], ids[3], ids[4]);

        run(&world.rt()).expect("run");

        for id in ids {
            assert_eq!(world.phase(id), FilePhase::Deduped);
        }
        assert_eq!(world.canonical_of(e1), Some(e1));
        assert_eq!(world.canonical_of(e2), Some(e1));
        assert_eq!(world.canonical_of(e3), Some(e1));
        assert_eq!(world.canonical_of(u1), Some(u1));
        assert_eq!(world.canonical_of(u2), Some(u2));
        assert_eq!(
            world.db.count_check_with_canonical_completed().expect("check flags"),
            0
        );
    }

    #[test]
    fn run_multiround_group_completes() {
        // {A,B,C}: A differs from B and C (two comparisons), B == C. Round 2
        // elects B and re-compares C against B — the I1 end-to-end repro.
        let world = TestWorld::new();
        let bc = pattern(512 * 1024, 5);
        let a_alt = pattern(512 * 1024, 3);
        let id_a = world.add_file("a.bin", &a_alt);
        let id_b = world.add_file("b.bin", &bc);
        let id_c = world.add_file("c.bin", &bc);
        world.db.apply_no_filter_archive().expect("filter");
        seed_rows(&world, Vec::from([(id_a, 42), (id_b, 42), (id_c, 42)]));

        run(&world.rt()).expect("run");

        assert_eq!(world.phase(id_a), FilePhase::Deduped);
        assert_eq!(world.phase(id_b), FilePhase::Deduped);
        assert_eq!(world.phase(id_c), FilePhase::Deduped);
        assert_eq!(world.canonical_of(id_a), Some(id_a));
        assert_eq!(world.canonical_of(id_b), Some(id_b));
        assert_eq!(world.canonical_of(id_c), Some(id_b));
        assert_eq!(
            world.db.count_check_with_canonical_completed().expect("check flags"),
            0
        );
    }

    #[test]
    fn run_fail_fast_on_errored_group() {
        if geteuid().is_root() {
            return;
        }
        let payload = pattern(512 * 1024, 5);
        let build = || {
            let world = TestWorld::new();
            let id_a = world.add_file("a.bin", &payload);
            let id_b = world.add_file("b.bin", &payload);
            world.db.apply_no_filter_archive().expect("filter");
            seed_rows(&world, Vec::from([(id_a, 7), (id_b, 7)]));
            std::fs::set_permissions(&world.path("b.bin"), std::fs::Permissions::from_mode(0))
                .expect("chmod 000");
            (world, id_a, id_b)
        };

        let (mut world_ff, id_a, id_b) = build();
        world_ff.config.process.fail_fast = true;
        let res = run(&world_ff.rt());
        assert!(matches!(res, Err(Error::Config(_))));

        let (world_ok, _id_a, _id_b) = build();
        run(&world_ok.rt()).expect("run tolerates the errored group");
        assert!(!world_ok.flag(_id_a, FileFlag::ErrorWhileDedup));
        assert!(world_ok.flag(_id_b, FileFlag::ErrorWhileDedup));
        assert_eq!(world_ok.canonical_of(_id_b), None);
        assert_eq!(world_ok.phase(_id_a), FilePhase::Deduped);
        assert_eq!(world_ok.phase(_id_b), FilePhase::Deduped);
    }

    fn run_interrupt_world() -> (TestWorld, Vec<FileId>) {
        let mut world = TestWorld::new();
        world.config.process.io_jobs = 1;
        let big = pattern(16 * 1024 * 1024, 3);
        let mid = pattern(16 * 1024 * 1024, 5);
        let id_b1 = world.add_file("big1.bin", &big);
        let id_b2 = world.add_file("big2.bin", &big);
        let id_s1 = world.add_file("s1.bin", &mid);
        let id_s2 = world.add_file("s2.bin", &mid);
        world.db.apply_no_filter_archive().expect("filter");
        seed_rows(&world, Vec::from([
            (id_b1, 1), (id_b2, 1),
            (id_s1, 2), (id_s2, 2),
        ]));
        (world, Vec::from([id_b1, id_b2, id_s1, id_s2]))
    }

    #[test]
    fn run_graceful_interrupt_mid_run_resume_completes() {
        // Interrupt machinery at the loop level (bar pools the test owns; the
        // worker finishes its in-flight pair, then stops between files). The
        // `length` bar signal is the live tell: the dedup worker sets it per
        // pair and never increments position.
        let (world, ids) = run_interrupt_world();
        prepare_for_loop(&world);

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh2 = world.shutdown.clone();
        let bar_obs = bar.clone();
        let trigger = thread::spawn(move || {
            // The bar starts at `Some(0)`; a *positive* length means the
            // worker is processing a pair (`set_length(candidate_size)`), so
            // at least one outcome is in flight when we stop it.
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

        let (fail_fast, completed) = run_loop(&world, bar, 4);
        trigger.join().expect("join trigger");
        world.progress.drop_thread_bars();

        assert_eq!(fail_fast, false);
        // The interrupt may land before any outcome is applied (compare_one's
        // own between-files gate) or after one; both are valid stops. The
        // unstarted second group guarantees leftovers.
        // `dedup_progress` survives the interrupt (dropped on success only).
        assert!(world.db.count_pending_dedup_groups().expect("pending") >= 1);

        // Resume with a fresh shutdown through `run`: everything still pending
        // completes.
        let sh3 = Shutdown::detached();
        run(&world.rt_with(&sh3)).expect("resume run");
        for id in ids {
            assert_eq!(world.phase(id), FilePhase::Deduped);
        }
        assert_eq!(world.db.count_check_with_canonical_completed().expect("check flags"), 0);
    }

    #[test]
    fn run_force_interrupt_mid_run_falls_through_interrupted() {
        // Force aborts the worker mid-compare (I2: `is_interrupted()` covers
        // force, so the phase falls through to `Err(Interrupted)` — pinned at
        // the run level by `run_interrupt_before_start_returns_interrupted`).
        let (world, ids) = run_interrupt_world();
        prepare_for_loop(&world);

        world.progress.create_thread_bars(BarKind::Bytes, 1);
        let bar = world.progress.thread_bar(0);
        let sh2 = world.shutdown.clone();
        let bar_obs = bar.clone();
        let trigger = thread::spawn(move || {
            for _ in 0..20_000 {
                if let Some(len) = bar_obs.length() {
                    if len > 0 {
                        sh2.request_force();
                        return;
                    }
                }
                thread::sleep(Duration::from_millis(1));
            }
            sh2.request_force();
        });

        let (fail_fast, _completed) = run_loop(&world, bar, 4);
        trigger.join().expect("join trigger");
        world.progress.drop_thread_bars();

        assert_eq!(fail_fast, false);
        assert!(world.db.count_pending_dedup_groups().expect("pending") >= 1);

        let sh3 = Shutdown::detached();
        run(&world.rt_with(&sh3)).expect("resume run");
        for id in ids {
            assert_eq!(world.phase(id), FilePhase::Deduped);
        }
        assert_eq!(world.db.count_check_with_canonical_completed().expect("check flags"), 0);
    }

    #[test]
    fn run_interrupt_before_start_returns_interrupted() {
        let world = TestWorld::new();
        let payload = pattern(512 * 1024, 3);
        world.add_file("a.bin", &payload);
        world.add_file("b.bin", &payload);
        world.db.apply_no_filter_archive().expect("filter");
        seed_rows(&world, Vec::from([(FileId(1), 7), (FileId(2), 7)]));
        world.shutdown.request_graceful();

        let res = run(&world.rt());

        assert!(matches!(res, Err(Error::Interrupted)));
        // The group tables survive for the resume.
        assert_eq!(world.db.count_pending_dedup_groups().expect("pending"), 1);
    }

    #[test]
    fn run_eager_parametrized() {
        let payload = pattern(512 * 1024, 5);
        let (mut world, specs) = seed_dedup_world(&payload, 9);
        world.config.filter.eager_filter = true;
        world.seed_eager();
        let ids = specs.into_iter().map(|(id, _)| id).collect::<Vec<FileId>>();
        let (e1, e2, e3, u1, u2) = (ids[0], ids[1], ids[2], ids[3], ids[4]);

        run(&world.rt()).expect("run");

        for id in ids {
            assert_eq!(world.phase(id), FilePhase::Deduped);
        }
        assert_eq!(world.canonical_of(e1), Some(e1));
        assert_eq!(world.canonical_of(e2), Some(e1));
        assert_eq!(world.canonical_of(e3), Some(e1));
        assert_eq!(world.canonical_of(u1), Some(u1));
        assert_eq!(world.canonical_of(u2), Some(u2));
        assert_eq!(
            world.db.count_check_with_canonical_completed().expect("check flags"),
            0
        );
    }

    // =========================================================================
    // Dedup-mode corpus: the same rows are seeded for every `--dedup-mode`, so
    // the three strategies can be compared 1:1. Distinct `(sha1, size)` keys
    // keep the groups apart; ineligible rows (non-file / no sha / hash error /
    // filter-excluded) must be promoted to `deduped` with canonical NULL in
    // every mode.
    //
    // Groups:
    //   singleton — unique content → its own canonical
    //   lone      — same (sha1, size), different bytes → lone-promotion (both self)
    //   pair      — same content → canonical + child
    //   quad      — X,X,Y,Y (X != Y) → two canonicals (C1, C3) each with a child
    //   errgrp    — E1==E2, E3 unreadable → canonical + child + errored member
    //   hard      — same (sha1, size, dev, inode) hard-link pair
    // =========================================================================

    struct ModeCorpus {
        non_file: FileId,
        no_sha: FileId,
        sha_err: FileId,
        filtered: FileId,
        singleton: FileId,
        lone_a: FileId,
        lone_b: FileId,
        pair_a: FileId,
        pair_b: FileId,
        quad_a: FileId,
        quad_b: FileId,
        quad_c: FileId,
        quad_d: FileId,
        err_a: FileId,
        err_b: FileId,
        err_c: FileId,
        hard_a: FileId,
        hard_b: FileId,
    }

    fn all_ids(c: &ModeCorpus) -> Vec<FileId> {
        Vec::from([
            c.non_file, c.no_sha, c.sha_err, c.filtered,
            c.singleton,
            c.lone_a, c.lone_b,
            c.pair_a, c.pair_b,
            c.quad_a, c.quad_b, c.quad_c, c.quad_d,
            c.err_a, c.err_b, c.err_c,
            c.hard_a, c.hard_b,
        ])
    }

    /// Seed the shared mode corpus. The unreadable `err_c` file is always
    /// chmod 000; tests that byte-read it must root-skip. Whether hardlink
    /// detection runs is the caller's `config.indexing` choice.
    fn seed_mode_corpus(world: &TestWorld) -> ModeCorpus {
        let x = pattern(512 * 1024, 3);       // quad content X
        let y = pattern(512 * 1024, 4);       // quad content Y (same length)
        let lone_p = pattern(128 * 1024, 7);  // lone_a
        let lone_q = pattern(128 * 1024, 8);  // lone_b (same length, different bytes)
        let pair_p = pattern(64 * 1024, 9);
        let err_p = pattern(64 * 1024, 10);
        let hard_p = pattern(64 * 1024, 11);

        let singleton = world.add_file("singleton.bin", &pattern(1024, 1));
        let lone_a = world.add_file("lone-a.bin", &lone_p);
        let lone_b = world.add_file("lone-b.bin", &lone_q);
        let pair_a = world.add_file("pair-a.bin", &pair_p);
        let pair_b = world.add_file("pair-b.bin", &pair_p);
        let quad_a = world.add_file("quad-a.bin", &x);
        let quad_b = world.add_file("quad-b.bin", &x);
        let quad_c = world.add_file("quad-c.bin", &y);
        let quad_d = world.add_file("quad-d.bin", &y);
        let err_a = world.add_file("err-a.bin", &err_p);
        let err_b = world.add_file("err-b.bin", &err_p);
        let err_c = world.add_file("err-c.bin", &err_p);
        let hard_a = world.add_file("hard-a.bin", &hard_p);
        let hard_b = world.add_file("hard-b.bin", &hard_p);
        let non_file = world.add_file("non-file.bin", &pattern(1024, 2));
        let no_sha = world.add_file("no-sha.bin", &pattern(1024, 3));
        let sha_err = world.add_file("sha-err.bin", &pattern(1024, 4));
        let filtered = world.add_file("filtered.bin", &pattern(1024, 5));

        world.db.apply_no_filter_archive().expect("filter");

        // Eligible rows get a digest; groups share one `(sha1, size)` key.
        world.seed_sha1(singleton, 0x11);
        world.seed_sha1(lone_a, 0x22); world.seed_sha1(lone_b, 0x22);
        world.seed_sha1(pair_a, 0x33); world.seed_sha1(pair_b, 0x33);
        world.seed_sha1(quad_a, 0x44); world.seed_sha1(quad_b, 0x44);
        world.seed_sha1(quad_c, 0x44); world.seed_sha1(quad_d, 0x44);
        world.seed_sha1(err_a, 0x55); world.seed_sha1(err_b, 0x55);
        world.seed_sha1(err_c, 0x55);
        world.seed_sha1(hard_a, 0x77); world.seed_sha1(hard_b, 0x77);
        world.seed_sha1(sha_err, 0x66);

        world.db.with_transaction(|conn| {
            // Ineligible rows.
            conn.execute(
                "UPDATE files SET ftype = 'dir' WHERE id = :id",
                named_params! { ":id": non_file.0 },
            ).expect("make non-file");
            conn.execute(
                "UPDATE files SET sha1 = NULL WHERE id = :id",
                named_params! { ":id": no_sha.0 },
            ).expect("null sha");
conn.execute(
                "UPDATE files SET include_reason_archive = 0 WHERE id = :id",
                named_params! { ":id": filtered.0 },
            ).expect("exclude from filter");
            // ErrorWhileHash: the row never produced a usable digest.
            assert_eq!(1, set_file_flag(conn, sha_err, FileFlag::ErrorWhileHash, true)?);
            // Hard-link pair: same (dev, inode).
            conn.execute(
                "UPDATE files SET dev = 7, inode = 99 WHERE id IN (:a, :b)",
                named_params! { ":a": hard_a.0, ":b": hard_b.0 },
            ).expect("seed dev/inode");
            Ok(())
        }).expect("corpus mutations");

        std::fs::set_permissions(&world.path("err-c.bin"), std::fs::Permissions::from_mode(0))
            .expect("chmod 000 err-c");

        ModeCorpus {
            non_file, no_sha, sha_err, filtered,
            singleton,
            lone_a, lone_b,
            pair_a, pair_b,
            quad_a, quad_b, quad_c, quad_d,
            err_a, err_b, err_c,
            hard_a, hard_b,
        }
    }

    fn assert_ineligible(world: &TestWorld, ids: [&FileId; 4]) {
        for id in ids {
            let canon = world.canonical_of(*id);
            assert_eq!(canon, None, "ineligible rows must have no canonical");
            assert_eq!(world.phase(*id), FilePhase::Deduped);
        }
    }

    #[test]
    fn mode_regular_corpus_dedupes_by_compare() {
        if geteuid().is_root() {
            return;
        }
        let mut world = TestWorld::new();
        world.config.indexing.no_hardlink_detection = false;
        world.config.pipeline.dedup_mode = DedupMode::Regular;
        let c = seed_mode_corpus(&world);
        run(&world.rt()).expect("run regular mode");

        assert_ineligible(&world, [&c.non_file, &c.no_sha, &c.sha_err, &c.filtered]);

        // Unique content: its own canonical (singleton promote sets id).
        assert_eq!(world.canonical_of(c.singleton), Some(c.singleton));

        // Same (sha1, size) but binary different: the canonical is retired and
        // the lone survivor is re-elected -> both end self-canonical.
        assert_eq!(world.canonical_of(c.lone_a), Some(c.lone_a));
        assert_eq!(world.canonical_of(c.lone_b), Some(c.lone_b));

        // Same content: canonical + child.
        assert_eq!(world.canonical_of(c.pair_a), Some(c.pair_a));
        assert_eq!(world.canonical_of(c.pair_b), Some(c.pair_a));

        // X,X,Y,Y -> two canonicals (C1, C3), each with one child.
        assert_eq!(world.canonical_of(c.quad_a), Some(c.quad_a));
        assert_eq!(world.canonical_of(c.quad_b), Some(c.quad_a));
        assert_eq!(world.canonical_of(c.quad_c), Some(c.quad_c));
        assert_eq!(world.canonical_of(c.quad_d), Some(c.quad_c));

        // canonical + child + errored member.
        assert_eq!(world.canonical_of(c.err_a), Some(c.err_a));
        assert_eq!(world.canonical_of(c.err_b), Some(c.err_a));
        assert_eq!(world.canonical_of(c.err_c), None);
        assert!(world.flag(c.err_c, FileFlag::ErrorWhileDedup));

        // Same (dev, inode) hard links collapse to the min id.
        assert_eq!(world.canonical_of(c.hard_a), Some(c.hard_a));
        assert_eq!(world.canonical_of(c.hard_b), Some(c.hard_a));

        for id in all_ids(&c) {
            assert_eq!(world.phase(id), FilePhase::Deduped);
        }
        assert_eq!(world.db.count_check_with_canonical_completed().expect("check flags"), 0);
    }

    #[test]
    fn mode_hash_corpus_trusts_digest() {
        let mut world = TestWorld::new();
        world.config.pipeline.dedup_mode = DedupMode::Hash;
        let c = seed_mode_corpus(&world);
        run(&world.rt()).expect("run hash mode");

        assert_ineligible(&world, [&c.non_file, &c.no_sha, &c.sha_err, &c.filtered]);

        // Each (sha1, size) group has exactly one canonical: min(id).
        assert_eq!(world.canonical_of(c.singleton), Some(c.singleton));

        assert_eq!(world.canonical_of(c.lone_a), Some(c.lone_a));
        assert_eq!(world.canonical_of(c.lone_b), Some(c.lone_a));

        assert_eq!(world.canonical_of(c.pair_a), Some(c.pair_a));
        assert_eq!(world.canonical_of(c.pair_b), Some(c.pair_a));

        assert_eq!(world.canonical_of(c.quad_a), Some(c.quad_a));
        for id in [c.quad_b, c.quad_c, c.quad_d] {
            assert_eq!(world.canonical_of(id), Some(c.quad_a));
        }

        assert_eq!(world.canonical_of(c.err_a), Some(c.err_a));
        for id in [c.err_b, c.err_c] {
            assert_eq!(world.canonical_of(id), Some(c.err_a));
        }

        assert_eq!(world.canonical_of(c.hard_a), Some(c.hard_a));
        assert_eq!(world.canonical_of(c.hard_b), Some(c.hard_a));

        for id in all_ids(&c) {
            assert_eq!(world.phase(id), FilePhase::Deduped);
        }
        assert_eq!(world.db.count_check_with_canonical_completed().expect("check flags"), 0);
    }

    #[test]
    fn mode_none_corpus_collapses_only_hardlinks() {
        let mut world = TestWorld::new();
        world.config.indexing.no_hardlink_detection = false;
        world.config.pipeline.dedup_mode = DedupMode::None;
        let c = seed_mode_corpus(&world);
        run(&world.rt()).expect("run none mode");

        assert_ineligible(&world, [&c.non_file, &c.no_sha, &c.sha_err, &c.filtered]);

        // No content dedup: content-identical files with different inodes are
        // each their own canonical...
        for id in [c.singleton, c.lone_a, c.lone_b, c.pair_a, c.pair_b,
                   c.quad_a, c.quad_b, c.quad_c, c.quad_d,
                   c.err_a, c.err_b, c.err_c] {
            assert_eq!(world.canonical_of(id), Some(id));
        }
        // ...the only collapsed group is the (sha1, size, dev, inode) hard link.
        assert_eq!(world.canonical_of(c.hard_a), Some(c.hard_a));
        assert_eq!(world.canonical_of(c.hard_b), Some(c.hard_a));

        for id in all_ids(&c) {
            assert_eq!(world.phase(id), FilePhase::Deduped);
        }
        assert_eq!(world.db.count_check_with_canonical_completed().expect("check flags"), 0);
    }

    #[test]
    fn mode_none_no_hardlink_detection_all_self_canonical() {
        let mut world = TestWorld::new();
        world.config.indexing.no_hardlink_detection = true;
        world.config.pipeline.dedup_mode = DedupMode::None;
        let c = seed_mode_corpus(&world);
        run(&world.rt()).expect("run none mode without hardlink detection");

        assert_ineligible(&world, [&c.non_file, &c.no_sha, &c.sha_err, &c.filtered]);

        // No grouping at all: every eligible file is self-canonical, including
        // the hard-link pair (which now share no canonical).
        for id in [c.singleton, c.lone_a, c.lone_b, c.pair_a, c.pair_b,
                   c.quad_a, c.quad_b, c.quad_c, c.quad_d,
                   c.err_a, c.err_b, c.err_c,
                   c.hard_a, c.hard_b] {
            assert_eq!(world.canonical_of(id), Some(id));
        }

        for id in all_ids(&c) {
            assert_eq!(world.phase(id), FilePhase::Deduped);
        }
        assert_eq!(world.db.count_check_with_canonical_completed().expect("check flags"), 0);
    }
}
