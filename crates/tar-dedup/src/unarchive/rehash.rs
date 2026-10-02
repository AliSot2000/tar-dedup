//! Rehash: verify extract-cache payloads against catalog SHA-1 digests.

use crate::common::{at_least_one_running, io_buffer};
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
use std::time::Duration;

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

    let is_running = || {
        at_least_one_running(&thread_handles.iter().collect())
    };

    let counts = handle_send_receive_loop(rt, work_s, out_r, is_running)?;
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
/// separate function.
fn handle_send_receive_loop(
    rt: &ExtractRTArgs,
    send: Sender<StrippedRecord>,
    recv: Receiver<Option<RehashOutcome>>,
    one_running: impl Fn() -> bool)
    -> Result<RehashCounts> {
    // Feed cursor over `rehash_queue`: `queue_index` is the last consumed queue
    // position. The pull filters to still-pending rows (phase predicate), so a
    // file already handed to a worker (or rehashed on a previous run) is never
    // re-pulled.
    let mut recorder = Recorder::new(rt.db, !rt.config.process.no_errors);

    let mut queue_index = 0u64;
    let mut feed_buf = Vec::<(u64, StrippedRecord)>::new();
    let mut feed_idx = 0usize;
    let mut feed_exhausted = false;
    #[warn(unused_assignments)]
    let mut busy = false;

    let mut dequeue_total = 0u64;
    let mut exited_workers = 0u64;
    let mut feed_total = 0u64;
    let mut counts = RehashCounts { matches: 0, mismatches: 0, errors: 0 };
    let mut pending_out = Vec::<RehashOutcome>::new();

    let mut apply_chunk = |pending: &mut Vec<RehashOutcome>|
        -> Result<()> {
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

    let mut drain_chunk = |
        is_busy: &mut bool, drain_override: bool,
        dequeue: &mut u64, exit: &mut u64|
        -> Result<()> {
        // Drain finished outcomes into a small batch.
        while pending_out.len() < DRAIN_CHUNK {
            match recv.try_recv() {
                Ok(Some(outcome)) => {
                    pending_out.push(outcome);
                    *is_busy = true;
                    *dequeue += 1;
                }
                Ok(None) => *exit += 1,
                Err(_) => break
            }
        }
        if pending_out.len() >= DRAIN_CHUNK || drain_override {
            apply_chunk(&mut pending_out)?;
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
        if feed_idx == feed_buf.len() && !feed_exhausted {
            feed_buf = rt.db.pull_pending_rehash_rows::<StrippedRecord>(
                queue_index, FEED_CHUNK as u64
            )?;
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
        if feed_exhausted && feed_idx == feed_buf.len()
            || !one_running() {
            break;
        }
        if !busy { thread::sleep(Duration::from_millis(10)); }
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
        if rt.shutdown.is_interrupted()
            || dequeue_total == feed_total
            || !one_running()
            || exited_workers == rt.config.process.effective_jobs() as u64 {
            break;
        }
        drain_chunk(&mut busy, false, &mut dequeue_total, &mut exited_workers)?;
        if !busy { thread::sleep(Duration::from_millis(10)); }
    }
    drain_chunk(&mut busy, true, &mut dequeue_total, &mut exited_workers)?;
    drop(recv);
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