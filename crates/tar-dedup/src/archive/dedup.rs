use std::path::Path;
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender, bounded};
use indicatif::ProgressBar;
use std::fs::File;
use std::io::Read;
use std::mem::take;

use crate::archive::ArchiveRTArgs;
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
    let ineligible = rt.db.promote_non_ineligible_entries_to_dedup(eager_filter)?;
    let skipped_singleton = rt.db.promote_singleton_filtered_to_deduped(eager_filter)?;
    // The bulk skips above left the phase; credit the global for each of them.
    rt.progress.inc_global(ineligible);
    rt.progress.inc_global(skipped_singleton);

    let prev_phase = if eager_filter {
        FilePhase::Hashed
    } else {
        FilePhase::Filtered
    };

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
        sanity_check_flags(rt.db)?;
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

    let one_running = || at_least_one_running(&thread_handles.iter().collect());
    let (fail_fast_hit, completed) = run_enqueue_dequeue_loop_dedup(
        &rt, work_s, out_r, one_running
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
            let leftover = rt.db.count_files_in_phase(prev_phase)?;
            if leftover != 0 {
                panic!(
                    "dedup finished with {leftover} file(s) still in {} \
                    (expected 0 after skips + rounds)",
                    prev_phase.as_str()
                );
            }
            sanity_check_flags(rt.db)?;
            rt.db.drop_temp_dedup_table()?;
            tracing::info!("dedup complete");
            Ok(())
        }
    }
}

/// Function performs the stepping of the loop. Deals with fetching data from the db,
/// enqueueing the data in the queue, retrieving the results from the queue and stepping the state
/// of the db.
fn run_enqueue_dequeue_loop_dedup(
    rt: &ArchiveRTArgs,
    send: Sender<ComparePair>,
    recv: Receiver<Option<CompareOutcome>>,
    one_running: impl Fn() -> bool)
    -> Result<(bool, u64)> {

    // Feed cursor over the candidate space. `dedup_inflight` (TEMP) marks pairs
    // handed out but not yet applied, so the scan can restart at id 0 any time
    // without re-comparing an in-flight pair.
    let eager_filter = rt.config.filter.eager_filter;
    let fail_fast = rt.config.process.fail_fast;
    let mut recorder = Recorder::new(rt.db, !rt.config.process.no_errors);

    // Feeder running variables
    let mut last_candidate = 0u64;
    let mut feed_buf = Vec::<(StrippedRecord, StrippedRecord)>::new();
    let mut feed_i = 0usize;

    // Loop runtime state
    let mut feed_total = 0u64;
    let mut exited_threads = 0u64;
    let mut dequeued_total = 0u64;
    let mut completed = 0u64;
    let mut fail_fast_hit = false;
    let mut pending_out = Vec::<CompareOutcome>::new();
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

    // Run the send receive loop
    loop {
        busy = false;
        if rt.shutdown.is_interrupted() {
            break;
        }
        // Handle dequeu side.
        drain_chunk(&mut busy, &mut exited_threads, &mut dequeued_total, false)?;

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
        // 3) Feed the pipeline. When the candidate slice runs out, restart the
        //    scan from the top: `dedup_inflight` + the check/promoted filters
        //    make re-listing exactly-once, and freshly transitioned groups are
        //    picked up by the fresh pull.
        if feed_i == feed_buf.len() {
            feed_buf = rt.db.list_pending_comparisons::<StrippedRecord>(
                eager_filter, last_candidate, FEED_CHUNK as u64
            )?;
            feed_i = 0;
            if feed_buf.is_empty() {
                break;
            } else {
                last_candidate = feed_buf[feed_buf.len() - 1].0.id.0 as u64;
            }
        }
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
        if !busy {
            thread::sleep(Duration::from_millis(1));
        }
    }
    // Cut the feed side; idle workers end their receive loop.
    drop(send);

    // Drain the remaining tasks scheduled to the workers are drained
    loop {
        busy = false;
        if rt.shutdown.is_interrupted() {
            break;
        }
        if dequeued_total == feed_total {
            break;
        }
        if exited_threads == rt.config.process.io_jobs as u64 {
            break;
        }
        if !one_running() {
            break;
        }
        drain_chunk(&mut busy, &mut exited_threads, &mut dequeued_total, true)?;
        if !busy {
            thread::sleep(Duration::from_millis(1));
        }
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
    detect_hardlinks: bool,
) {
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

// TODO Different Error.
/// Rerun the count_check_with_canonical_completed and return an error if the count is not 0.
fn sanity_check_flags(db: &Database) -> Result<()> {
    let n = db.count_check_with_canonical_completed()?;
    if n != 0 {
        return Err(Error::Config(format!(
            "dedup sanity check failed: {n} file(s) still have CheckWithCanonicalCompleted set"
        )));
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
