//! Shared helpers used by both archive and unarchive pipelines.

pub mod cleanup;
pub mod files;
pub mod filter;
pub mod perms;
pub mod start;
pub mod transform;
pub mod xattr;

use crate::error::Result;
use crate::shutdown::Shutdown;
use crossbeam_channel::{Receiver, Sender};
use std::mem::take;
use std::thread;
use std::thread::JoinHandle;
use std::time::Duration;

// Constants reused across the project that need to be coherent.

/// Name for the first database that is added to an archive to record what was considered initially.
pub const SNAPSHOT_INIT_TAR_NAME: &str = "manifest.sqlite";

/// Name for any subsequent database added to the archive which are used to store the progress of
/// appending files to the archive.
pub const SNAPSHOT_TAR_NAME: &str = "snapshot.sqlite";

/// To ensure the program is responsive, we need to periodically check, if the user interrupted us.
/// This is the stepsize during read / write operations between successive checks of the program
/// status.
pub const IO_BUF_SIZE: usize = 1024 * 1024 * 4;

/// Tar read chunk size during archive (keep xz fed without huge resident buffers).
const ARCHIVE_IO_BUF_SIZE: usize = 4 * 1024 * 1024;

pub fn io_buffer() -> Vec<u8> {
    vec![0u8; IO_BUF_SIZE]
}

pub fn archive_io_buffer() -> Vec<u8> {
    vec![0u8; ARCHIVE_IO_BUF_SIZE]
}

/// When processing files, file system entries, ... we take the precaution not to load too much
/// into ram. Worst case Estimate is 16kiB / Entry, so we try to be conservative with 100'000 as
/// a batch size
pub const DEFAULT_BATCH_SIZE: u64 = 100_000;

/// Number of ErrorRecordDrafts at a time in ram before attempting to auto flush;
pub const DEFAULT_AUTO_FLUSH_LIMIT: u64 = 10_000;

/// Perform the batched loop with a step id. Arguments work as follows:
/// [`new_id`]: Function must return the lower bound for ids. Typically 0, since we start id at 1
/// [`get_entries`]: Function that gets the next batch starting with last_id, u64 is for batch_size
/// [`get_id`]: Function gets the id from an entry. This function MUST return an id.
/// [`process_entries`]: Once the entries are ready, hand control to "loop body" function
/// [`batch_size`]: Determines the max size of batches from the get_entries function.
pub fn batched_stepped_loop<ID, ENTRY, I, G, GI, P>(
    batch_size: u64,
    mut init_id: I,
    mut get_entries: G,
    mut get_id: GI,
    mut process_entries: P)
    -> Result<()>
where
    I: FnMut() -> ID,
    G: FnMut(&ID, u64) -> Result<Vec<ENTRY>>,
    GI: FnMut(&ENTRY) -> ID,
    P: FnMut(Vec<ENTRY>) -> Result<()> {

    let mut last_id: ID = init_id();
    loop {
        let entries = get_entries(&last_id, batch_size)?;
        if entries.is_empty() { break }
        let vec_last = entries
            .last()
            .expect("PRECONDITION FAILED: At least one element expect.");
        last_id = get_id(vec_last);

        process_entries(entries)?;
    }
    Ok(())
}

pub fn batched_loop<ENTRY, G, P>(
    mut get_entries: G,
    batch_size: u64,
    mut process_entries: P)
    -> Result<()>
where
    G: FnMut(u64) -> Result<Vec<ENTRY>>,
    P: FnMut(Vec<ENTRY>) -> Result<()> {

    loop {
        let entries = get_entries(batch_size)?;
        if entries.is_empty() { break }
        process_entries(entries)?;
    }
    Ok(())
}

pub fn at_least_one_running(threads: &Vec<&JoinHandle<()>>) -> bool {
    for handle in threads {
        if !handle.is_finished() {
            return true;
        }
    }
    false
}

/// Run the shared producer/consumer send/receive loop.
///
/// The phase drives nothing but the closures and the channels:
/// - [`pull`]:   next work slice; empty = the feed is exhausted.
/// - [`on_sent`]: observe the items handed to the channel (`dedup`'s in-flight
///   marker lives here). Called every feed round; an empty slice is valid and
///   must be a no-op (full channel, exhausted feed).
/// - [`tick`]:   per-iteration phase hook (the dedup FSM); `false` stops the
///   feed. Runs **after** the drain so applied outcomes are visible to it.
/// - [`apply`]:  commit a batch of outcomes (ingest + recorder + progress).
///
/// Returns the number of committed outcomes.
///
/// Ordering per iteration is drain → tick → feed: applied results feed the
/// phase machine before the next work slice is pulled (dedup's demanded
/// coupling; for the batch phases `tick` is a no-op so the order is moot).
///
/// Shutdown handling: a pending interrupt (graceful *or* force) stops the feed,
/// but the loop then waits out every worker (graceful finishes in-flight work,
/// force aborts it, a panicked worker is not waited on) and drains the result
/// queue **to empty** before returning — no queued outcome is ever dropped, no
/// matter how big the in-flight files were.
pub fn send_receive_loop<W, O>(
    shutdown: &Shutdown,
    send: Sender<W>,
    recv: Receiver<Option<O>>,                                  // None == worker-exit marker
    mut handles: Vec<JoinHandle<()>>,
    drain_chunk: usize,
    apply_on_partial: bool,                                     // dedup: true (FSM needs every batch)
    mut pull:    impl FnMut() -> Result<Vec<W>>,
    mut on_sent: impl FnMut(&[W]) -> Result<()>,
    mut tick:    impl FnMut() -> Result<bool>,
    mut apply:   impl FnMut(&mut Vec<O>) -> Result<()>)
    -> Result<u64>
where
    W: Clone + Send,
    O: Send,
{
    let mut feed_buf = Vec::<W>::new();
    let mut feed_idx = 0usize;
    let mut feed_total = 0u64;
    let mut feed_exhausted = false;

    let mut dequeue_total = 0u64;
    let mut exited_workers = 0u64;
    let mut pending_out = Vec::<O>::new();
    #[warn(unused_assignments)]
    let mut busy = false;

    let mut drain = |
        drain_override: bool,
        is_busy: &mut bool,
        dequeue: &mut u64,
        exit: &mut u64| -> Result<()> {
        // Drain finished outcomes into a small batch.
        while pending_out.len() < drain_chunk {
            match recv.try_recv() {
                Ok(Some(outcome)) => {
                    pending_out.push(outcome);
                    *is_busy = true;
                    *dequeue += 1;
                }
                Ok(None) => *exit += 1,
                Err(_) => break,
            }
        }
        if pending_out.len() >= drain_chunk || drain_override {
            // INFO: Apply sets takes from pending_out s.t. is_empty() is true afterwards.
            apply(&mut pending_out)?;
            *is_busy = true;
        }
        Ok(())
    };

    // Phase 1 — feed. Any pending shutdown stops the feed: workers observe it
    // between files / in-flight and either finish or abort, so keeping the feed
    // open would only pile up rows nobody consumes (and on force could wedge
    // the loop in a full-channel retry).
    loop {
        if shutdown.is_interrupted() { break; }
        busy = false;
        // Drain first, then step the phase machine, then feed.
        drain(apply_on_partial, &mut busy, &mut dequeue_total, &mut exited_workers)?;
        if !tick()? { break; }
        // Refill whenever the feed buffer is drained. Safe to re-pull from a
        // fresh cursor every time: the batch phases advance a positional cursor
        // past handed-but-unapplied rows, and dedup's `dedup_inflight` (TEMP)
        // makes its from-scratch scan exactly-once — so the FSM can also create
        // *new* eligible rounds after the feed looked exhausted.
        if feed_idx == feed_buf.len() {
            feed_buf = pull()?;
            feed_idx = 0;
            feed_exhausted = feed_buf.is_empty();
        }
        let mut sent = Vec::<W>::new();
        while feed_idx < feed_buf.len() {
            let item = feed_buf[feed_idx].clone();
            match send.try_send(item.clone()) {
                Ok(_) => {
                    sent.push(item);
                    feed_total += 1;
                    feed_idx += 1;
                    busy = true;
                }
                Err(_) => break,
            }
        }
        // An empty handed batch is still handed over (e.g. a full channel or an
        // exhausted feed): `on_sent` is contractually a no-op on `[]`, like
        // `apply` already is — the loop does not police it.
        on_sent(&sent)?;
        // Every row fed and every handed-out outcome accounted for — or the
        // workers are gone (interrupt aborted the run / a worker panicked).
        if (feed_exhausted && feed_idx == feed_buf.len() && dequeue_total == feed_total)
            || !at_least_one_running(&handles.iter().collect()) {
            break;
        }
        if !busy {
            thread::sleep(Duration::from_millis(10));
        }
    }

    // Cut the feed side; idle workers end their receive loop.
    drop(send);

    // Phase 2 — wait for worker exit (no interrupt break; prio: never drop
    // progress, exit quickly). Graceful finishes in-flight files, force aborts
    // them; their already-computed outcomes are still drained and committed.
    loop {
        if exited_workers == handles.len() as u64
            || !at_least_one_running(&handles.iter().collect()) {
            break;
        }
        drain(false, &mut busy, &mut dequeue_total, &mut exited_workers)?;
        if !busy {
            thread::sleep(Duration::from_millis(4));
        }
    }

    // Phase 3 — join + drain-to-empty. Joining first makes `drop(recv)`
    // race-free (no worker can still `out.send`), and panicked workers join
    // instantly. Post-join the channel is stable, so draining to `is_empty()`
    // commits every queued outcome.
    for handle in take(&mut handles) {
        let _ = handle.join();
    }
    loop {
        drain(true, &mut busy, &mut dequeue_total, &mut exited_workers)?;
        if recv.is_empty() { break; }
    }
    drop(recv);
    Ok(dequeue_total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::bounded;

    /// Toy worker factory type: maps each `w` to outcome `w + 1`, then sends the
    /// `None` exit marker. Tests override the body for interrupt/panic shapes.
    type ToyWorker = Box<dyn FnOnce(Receiver<u64>, Sender<Option<u64>>) + Send>;

    fn map_worker() -> ToyWorker {
        Box::new(|work: Receiver<u64>, out: Sender<Option<u64>>| {
            loop {
                match work.recv() {
                    Ok(w) => out.send(Some(w + 1)).expect("toy worker out"),
                    Err(_) => break,
                }
            }
            out.send(None).expect("toy worker out");
        })
    }

    /// Drive `send_receive_loop` with a toy worker pool. Returns
    /// `(dequeue_total, applied, sent_log, batches)`.
    fn run_toy(
        shutdown: &Shutdown,
        workers: usize,
        make_worker: impl Fn() -> ToyWorker,
        items: Vec<u64>,
        work_cap: usize,
        drain_chunk: usize,
        apply_on_partial: bool,
        mut tick: impl FnMut() -> Result<bool>) -> (u64, Vec<u64>, Vec<u64>, Vec<Vec<u64>>) {
        let (work_s, work_r) = bounded::<u64>(work_cap);
        let (out_s, out_r) = bounded::<Option<u64>>(1024);
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let wr = work_r.clone();
            let os = out_s.clone();
            let worker = make_worker();
            handles.push(thread::Builder::new().name("toy-worker".into())
                .spawn(move || worker(wr, os))
                .expect("spawn toy worker"));
        }
        drop(work_r);
        drop(out_s);

        let mut offered = items;
        let mut fed = false;
        let mut sent_log = Vec::<u64>::new();
        let mut batches = Vec::<Vec<u64>>::new();
        let mut applied = Vec::<u64>::new();
        let dequeue = send_receive_loop(
            shutdown, work_s, out_r, handles,
            drain_chunk, apply_on_partial,
            || {
                if fed {
                    Ok(Vec::new())
                } else {
                    fed = true;
                    Ok(take(&mut offered))
                }
            },
            |sent: &[u64]| { sent_log.extend_from_slice(sent); Ok(()) },
            tick,
            |out: &mut Vec<u64>| {
                if !out.is_empty() {
                    batches.push(out.clone());
                    applied.extend_from_slice(out);
                }
                out.clear();
                Ok(())
            },
        ).expect("toy loop");
        (dequeue, applied, sent_log, batches)
    }

    fn sorted(mut v: Vec<u64>) -> Vec<u64> {
        v.sort_unstable();
        v
    }

    #[test]
    fn feeds_all_applies_in_batches() {
        let sh = Shutdown::detached();
        // 8 items through a cap-4 channel; drain_chunk=2 forces batched applies.
        let (dequeue, applied, sent_log, batches) = run_toy(
            &sh, 1, map_worker, (0..8).collect(), 4, 2, false, || Ok(true));

        assert_eq!(dequeue, 8);
        assert_eq!(sorted(applied), (1..=8).collect::<Vec<_>>());
        assert_eq!(sent_log, (0..8).collect::<Vec<_>>());
        // drained in exact 2-item commits (no ragged mid-flight apply)
        assert!(batches.iter().all(|b| b.len() == 2));
    }

    #[test]
    fn partial_final_batch_not_lost() {
        let sh = Shutdown::detached();
        // drain_chunk=2, 3 outcomes: the ragged tail must still be committed.
        let (dequeue, applied, _, _) = run_toy(
            &sh, 1, map_worker, vec![0, 1, 2], 4, 2, false, || Ok(true));

        assert_eq!(dequeue, 3);
        assert_eq!(sorted(applied), vec![1, 2, 3]);
    }

    #[test]
    fn graceful_preset_stops_immediately() {
        let sh = Shutdown::detached();
        sh.request_graceful();
        let (dequeue, applied, sent_log, _) = run_toy(
            &sh, 1, map_worker, (0..8).collect(), 4, 2, false, || Ok(true));

        assert_eq!(dequeue, 0);
        assert!(applied.is_empty());
        assert!(sent_log.is_empty());
    }

    #[test]
    fn graceful_mid_feed_drains_delivered_not_more() {
        let sh = Shutdown::detached();
        let mk = || Box::new({
            let sh = sh.clone();
            move |work: Receiver<u64>, out: Sender<Option<u64>>| {
                match work.recv() {
                    Ok(w) => {
                        out.send(Some(w + 1)).expect("toy worker out");
                        // Graceful lands while more rows sit in the feed
                        // buffer: everything delivered so far is applied,
                        // nothing further is fed.
                        sh.request_graceful();
                    }
                    Err(_) => (),
                }
                out.send(None).expect("toy worker out");
            }
        }) as ToyWorker;
        let (dequeue, applied, sent_log, _) = run_toy(
            &sh, 1, mk, (0..8).collect(), 4, 2, false, || Ok(true));

        // Exactly one outcome was delivered and it is applied; the feed stopped
        // well short of the full slice.
        assert_eq!(dequeue, 1);
        assert_eq!(applied, vec![1]);
        assert!(sent_log.len() < 8);
        assert!(sent_log.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn force_mid_inflight_discards_and_exits() {
        let sh = Shutdown::detached();
        let mk = || Box::new({
            let sh = sh.clone();
            move |work: Receiver<u64>, out: Sender<Option<u64>>| {
                sh.request_force();
                let _ = work.recv();
                out.send(None).expect("toy worker out");
            }
        }) as ToyWorker;
        let (dequeue, applied, sent_log, _) = run_toy(
            &sh, 1, mk, (0..8).collect(), 4, 2, false, || Ok(true));

        // The in-flight item produces no outcome and nothing is committed; the
        // loop still exits cleanly (no hang) after the workers are gone.
        assert_eq!(dequeue, 0);
        assert!(applied.is_empty());
        assert!(sent_log.len() >= 4);
    }

    #[test]
    fn panicked_worker_terminates_loop() {
        let sh = Shutdown::detached();
        let mk = || Box::new(|_: Receiver<u64>, _: Sender<Option<u64>>| -> () {
            panic!("toy worker crashed")
        }) as ToyWorker;
        let (dequeue, applied, _, _) = run_toy(
            &sh, 1, mk, (0..8).collect(), 4, 2, false, || Ok(true));

        // No None marker arrives; the loop must terminate via `is_finished`
        // and commit the (empty) results.
        assert_eq!(dequeue, 0);
        assert!(applied.is_empty());
    }

    #[test]
    fn drain_to_empty_on_exit() {
        let sh = Shutdown::detached();
        // The worker bursts more outcomes than the drain chunk, then exits; all
        // must be committed (phase-3 drain-to-empty covers the ragged tail).
        let mk = || Box::new(|work: Receiver<u64>, out: Sender<Option<u64>>| {
            let _ = work.recv();
            for i in 0..7 {
                out.send(Some(1000 + i)).expect("toy worker out");
            }
            out.send(None).expect("toy worker out");
        }) as ToyWorker;
        let (dequeue, applied, _, _) = run_toy(
            &sh, 1, mk, vec![0], 4, 6, false, || Ok(true));

        assert_eq!(dequeue, 7);
        assert_eq!(sorted(applied), (1000..1007).collect::<Vec<_>>());
    }

    #[test]
    fn on_sent_receives_only_handed_items() {
        let sh = Shutdown::detached();
        // The worker never consumes; the feed stops the moment the work channel
        // is full. Items pulled into the feed buffer but refused by a full
        // channel must never reach `on_sent`.
        let mk = || Box::new(|_: Receiver<u64>, out: Sender<Option<u64>>| {
            out.send(None).expect("toy worker out");
        }) as ToyWorker;
        let (dequeue, applied, sent_log, _) = run_toy(
            &sh, 1, mk, (0..8).collect(), 2, 2, false, || Ok(true));

        assert_eq!(sent_log, vec![0, 1]);   // only the two the cap-2 channel took
        assert_eq!(dequeue, 0);
        assert!(applied.is_empty());
    }

    #[test]
    fn tick_false_stops_feed() {
        let sh = Shutdown::detached();
        let mut calls = 0u64;
        let tick = move || { calls += 1; if calls < 2 { Ok(true) } else { Ok(false) } };
        let (dequeue, applied, sent_log, _) = run_toy(
            &sh, 1, map_worker, (0..8).collect(), 4, 2, false, tick);

        // One feed round happened; the tick then stopped further pulls.
        assert_eq!(dequeue, 4);
        assert_eq!(sorted(applied), vec![1, 2, 3, 4]);
        assert_eq!(sent_log, (0..4).collect::<Vec<_>>());
    }

    #[test]
    fn exit_marker_bookkeeping() {
        let sh = Shutdown::detached();
        // Two workers each send a `None` exit marker; the loop must count both
        // and commit every outcome before returning.
        let (dequeue, applied, _, _) = run_toy(
            &sh, 2, map_worker, (0..6).collect(), 4, 2, false, || Ok(true));

        assert_eq!(dequeue, 6);
        assert_eq!(sorted(applied), (1..=6).collect::<Vec<_>>());
    }
}