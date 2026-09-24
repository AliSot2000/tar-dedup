use crate::archive::ArchiveRTArgs;
use crate::common::files::warn_if_times_changed;
use crate::common::io_buffer;
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
    let (out_s, out_r) = bounded::<HashingOutcome>(OUT_CAPACITY);

    for i in 0..jobs {
        let bar = bars[i].clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let sh = shutdown.clone();
        let ps = page_size;
        thread::Builder::new()
            .name(format!("hash-worker-{i}").into())
            .spawn(move || hash_worker(bar, ps, sh, wr, os))
            .expect("spawn hash worker");
    }
    drop(work_r);
    drop(out_s);

    let completed = handle_send_receive_loop(&rt, work_s, out_r)?;
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
    rt: &ArchiveRTArgs, send: Sender<StrippedRecord>, recv: Receiver<HashingOutcome>)
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

    let mut drain_chunk = |is_busy: &mut bool, drain_override: bool|
                           -> Result<()> {
        // Drain finished outcomes into a small batch.
        while pending_out.len() < DRAIN_CHUNK {
            match recv.try_recv() {
                Ok(res) => {
                    pending_out.push(res);
                    *is_busy = true;
                    dequeue_total += 1;
                }
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

        drain_chunk(&mut busy, false)?;
        // Leave for dequeue loop.
        if feed_exhausted && feed_idx == feed_buf.len() {
            break;
        }
        if !busy {
            thread::sleep(Duration::from_millis(10));
        }
    }

    // Cut the feed side; idle workers end their receive loop.
    drop(send);

    // Progress to drain only
    loop {
        if rt.shutdown.is_interrupted() { break; }
        busy = false;
        drain_chunk(&mut busy, false)?;

        // Condition, everything is both enqueued and dequeud. Need to finish buffers.
        if queue_index == feed_total {
            break;
        }

        if !busy {
            thread::sleep(Duration::from_millis(10));
        }
    }

    // Definitively drain whatever the workers still produce (incl. after an
    // interrupt: workers finish their in-flight file, then the channel drops).
    drain_chunk(&mut busy, true)?;
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
    out: Sender<HashingOutcome>) -> () {
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
                out.send(res).expect("hash worker: result channel closed");
            }
            Err(_) => break
        }
    }
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
    if buf.len() < IO_BUF_SIZE {
        buf.resize_with(IO_BUF_SIZE, || 0u8);
    }

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
