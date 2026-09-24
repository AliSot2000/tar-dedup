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

    let pending = rt.db.count_pending_sparsify_candidates(rt.config.sparse.min_pages)?;
    rt.progress.set_phase_total(pending);
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

    let (completed, errored) = run_enqueue_dequeue_loop_sparsify(&rt, work_s, out_r, thread_handles)?;
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
        if feed_exhausted && feed_idx == feed_buf.len() {
            break;
        }
        if !at_least_one_running(&handles.iter().collect()) {
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
        if dequeue_total == feed_total {
            break;
        }
        if !at_least_one_running(&handles.iter().collect()) {
            break;
        }
        if exited_workers == rt.config.process.io_jobs as u64 {
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