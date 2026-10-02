# Plan: Unified `send_receive_loop` → `crates/tar-dedup/src/common.rs`

Status: **agreed, ready to implement** (2026-10-03). Supersedes the four per-phase loop
copies. Staging (user-confirmed): **Stage 1** = build the unified loop + generic tests +
adopt into hash/dedup/sparsify/rehash; **Stage 2** = rehash tests (`plans/rehash-tests.md`,
minus the superseded loop-mechanics items); **Stage 3** = rework the extract place
("materialize") loops to adopt it (out of scope here, recorded as a follow-up).

## Goal

One generic, closure-parameterized producer/consumer loop replaces:

- `archive/hash.rs::handle_send_receive_loop(rt, send, recv, one_running) -> Result<u64>`
- `archive/dedup.rs::run_enqueue_dequeue_loop_dedup(rt, send, recv, handles) -> Result<(bool, u64)>`
- `archive/sparsify.rs::run_enqueue_dequeue_loop_sparsify(rt, send, recv, handles) -> Result<(u64, u64)>`
- `unarchive/rehash.rs::handle_send_receive_loop(rt, send, recv, one_running) -> Result<RehashCounts>`

It is **not** tied to `ArchiveRTArgs`/`ExtractRTArgs` — it lives in `crate::common` and
depends only on `Shutdown`, `crate::error::Result`, crossbeam, and the thread primitives.
Everything phase-specific is exchanged through closures; unknown types are generics.

## Priorities (drive every decision)

1. **Keep the threads fed** — each iteration `try_send`s a full channel-worth of work.
2. **Never drop progress** — on shutdown (graceful *and* force), wait until all threads have
   exited, then drain the results queue **to empty** and commit. We do NOT exit early and
   abandon queued outcomes: e.g. rehash with multiple 10s-of-GB files that already completed
   and sit in the out-queue must be applied even under force.
3. **Exit quickly on shutdown** — workers observe shutdown themselves (graceful finishes
   in-flight, force aborts via `check_in_flight`), so "wait for exit" is bounded by in-flight
   work. A **panicked** thread is not waited on (`JoinHandle::is_finished()` backstop + join
   returns immediately for a dead thread); its already-sent outcomes are still drained.

## The unified function — `crates/tar-dedup/src/common.rs`

```rust
/// Run the shared producer/consumer send/receive loop. See plan headers for the
/// closure contracts; the phase drives nothing but the closures and the channels.
pub fn send_receive_loop<W, O>(
    shutdown: &Shutdown,
    send: Sender<W>,
    recv: Receiver<Option<O>>,             // None == worker-exit marker
    mut handles: Vec<thread::JoinHandle<()>>,
    feed_chunk: u64,
    drain_chunk: usize,
    apply_on_partial: bool,                // dedup: true (its FSM needs every batch applied)
    mut pull:    impl FnMut() -> Result<Vec<W>>,         // next work slice; empty = exhausted
    mut on_sent: impl FnMut(&[W]) -> Result<()>,         // dedup: mark_inflight(sent ids)
    mut tick:    impl FnMut() -> Result<bool>,           // per-iteration (FSM); false = stop feeding
    mut apply:   impl FnMut(&mut Vec<O>) -> Result<()>,  // ingest + recorder + progress + counters
) -> Result<u64>                                            // outcomes applied (== committed)
where
    W: Clone + Send,
    O: Send,
```

### Loop body

**Phase 1 — feed** (user order: push tasks, then drain):
```
loop {
    if shutdown.is_interrupted() { break; }
    // optional per-iteration phase hook (dedup FSM); false -> stop feeding
    if !tick()? { break; }
    busy = false;
    // 1) push tasks
    if feed_idx == feed_buf.len() {
        feed_buf = pull()?; feed_idx = 0;
        if feed_buf.is_empty() { feed_exhausted = true; }
    }
    let mut sent = Vec::new();
    while feed_idx < feed_buf.len() {
        match send.try_send(feed_buf[feed_idx].clone()) {
            Ok(_) => { feed_total += 1; busy = true; sent.push(feed_buf[feed_idx].clone()); feed_idx += 1 }
            Err(_) => break,
        }
    }
    if !sent.is_empty() { on_sent(&sent)?; }
    // 2) drain, in big batches (big transactions)
    drain_chunk(&mut busy, apply_on_partial, &mut dequeue_total, &mut exited_workers)?;
    if feed_exhausted && feed_idx == feed_buf.len() || !one_running() { break; }
    if !busy { thread::sleep(Duration::from_millis(10)); }
}
drop(send);
```
`one_running()` = `at_least_one_running(&handles.iter().collect())`.

**Phase 2 — wait for worker exit (NO interrupt break; prio 2/3):**
```
loop {
    if exited_workers == handles.len() || !one_running() { break; }
    drain_chunk(&mut busy, false, ...)?;
    if !busy { thread::sleep(Duration::from_millis(4)); }
}
```
- Graceful: in-flight files finish → outcomes applied as they arrive.
- Force: workers abort in-flight promptly (check_in_flight) → exit fast; their already-sent
  outcomes are still drained here.

**Phase 3 — join + drain-to-empty (the prio-2 guarantee):**
```
for h in take(&mut handles) { let _ = h.join(); }        // panicked threads join instantly
loop {
    drain_chunk(&mut busy, true, &mut dequeue_total, &mut exited_workers)?;
    if recv.is_empty() { break; }                        // post-join, no new sends can arrive
}
drop(recv);
Ok(dequeue_total)
```
Post-join the channel is stable, so a single drain-until-empty loop commits **all** queued
outcomes — this is the fix for the old force path dropping > `DRAIN_CHUNK` outcomes.

`drain_chunk` (module-private helper or inline closure; consumer of `apply`):
```
while pending_out.len() < drain_chunk {
    match recv.try_recv() {
        Ok(Some(o)) => { pending_out.push(o); busy = true; dequeue += 1; }
        Ok(None) => exit += 1,
        Err(_) => break,
    }
}
if pending_out.len() >= drain_chunk || override { apply(&mut pending_out)?; busy = true; }
```

### Semantics notes
- Exit condition uses `handles.len()` — removes the `effective_jobs()` vs `io_jobs`
  divergence.
- `tick()` runs drain-before-feed order (dedup's proven FSM coupling: apply → FSM → feed).
  For the batch phases `tick` is `|| Ok(true)` so the order is immaterial (user approved).
- Recorder is **phase-side**: constructed before the call, borrowed by `apply`, flushed (or
  auto-flushed, or Drop-flushed) after the call. The loop never sees it.
- Loop returns a single `u64` (outcomes applied). Richer counters (rehash
  matches/mismatches/errors, sparsify errored, dedup fail_fast_hit) are captured in the
  phase's closure state and read after the call.

## Closure contract per phase

| phase | W | O | pull | on_sent | tick | apply (recorder & progress inside) | counters read after call |
|---|---|---|---|---|---|---|---|
| hash | `StrippedRecord` | `HashingOutcome` | `pull_pending_hash_rows::<StrippedRecord>(cursor, feed_chunk)` (cursor in closure) | no-op | `\|\| Ok(true)` | `ingest_hash_outcome` + `record_hash_error` + `inc_both(n)` | return value = completed |
| rehash | `StrippedRecord` | `RehashOutcome` | rehash queue pull w/ cursor | no-op | `\|\| Ok(true)` | `ingest_rehash_outcome` + recorder for `Errored` + `inc_both(n)` | `matches/mismatches/errors` |
| sparsify | `StrippedRecord` | `SparseOutcome` | sparsify queue pull w/ cursor | no-op | `\|\| Ok(true)` | `ingest_sparsify_outcome` + `record_sparsify_error` + `inc_both(n)` | `completed`, `errored` |
| dedup | `ComparePair` | `CompareOutcome` | `list_pending_comparisons::<StrippedRecord>(eager, 0, feed_chunk)` fresh | `mark_inflight(sent ids)` | FSM (`searching_to_finished`, `finish_to_error`/fail_fast, `finish_to_done`, `finish_to_ready`, `ready_to_searching`) + `inc_both(promoted)`; `false` on fail_fast or no pending groups | `ingest_compare_outcome` + `record_dedup_error` (apply_on_partial = true) | `fail_fast_hit` |

Channels + worker handles are still created per phase in their `run()`, then handed to the
loop. Constants stay per-phase at the call site (`WORK_CAPACITY`/`OUT_CAPACITY` bound the
channels; `FEED_CHUNK`/`DRAIN_CHUNK` are loop args).

## Behavior deltas vs today (deliberate)

1. hash + rehash gain the worker **join** before `drop(recv)` (dedup/sparsify already had it)
   → kills the graceful `out.send`-after-`drop(recv)` race ("result channel closed" panic +
   lost outcome).
2. Phase-2 no longer breaks on `is_interrupted()`; it waits for worker exit and continues
   applying. Phase-3 drains **to empty**. Combined: force still aborts in-flight work but
   commits everything already in the results queue.
3. Loop return narrowed to `u64`; recorder flush moves to the phase; `run()` tails are
   unchanged in behavior and source their messages from captured counters + the return.

## Scope / change list

| File | Change |
|---|---|
| `src/common.rs` | add `send_receive_loop` (+ imports: `Shutdown`, `Result`, crossbeam `{Sender,Receiver,bounded}` not needed here, `std::mem::take`, `std::thread`, `Duration`) + `#[cfg(test)] mod tests` (matrix below). |
| `src/archive/hash.rs` | delete local loop + `is_running` closure; call `common::send_receive_loop(…)` with pull/apply closures; adapt `run_loop` test harness + drop the superseded loop tests. |
| `src/archive/dedup.rs` | delete `run_enqueue_dequeue_loop_dedup`; call shared loop with `list_pending_comparisons` pull, `mark_inflight` on_sent, FSM tick, `apply_on_partial=true`; adapt harness/tests. |
| `src/archive/sparsify.rs` | delete `run_enqueue_dequeue_loop_sparsify`; call shared loop; adapt harness/tests. |
| `src/unarchive/rehash.rs` | delete `handle_send_receive_loop`; call shared loop; adapt (rehash tests come in Stage 2). |
| `plans/rehash-tests.md` | status → Stage 2; loop-contract tests rewritten against the shared loop (drop the ones G1–G10 cover). |

## Generic test suite (`common.rs` bottom; toy `W=u64`, `O=u64`, no DB/progress)

Harness: a threaded "worker" mapping each `W`→`O` with programmable behavior; closures
capture into local `Vec`s (pull count, on_sent items, applied slices, tick calls, etc.);
`drain_chunk` kept tiny to exercise batching.

| # | Test | Pins |
|---|------|------|
| G1 | `feeds_all_applies_in_batches` | all fed → all applied in batched slices, returns N, exit markers counted |
| G2 | `partial_final_batch_not_lost` | `drain_chunk=2`, N=3 → ragged tail committed (final override) |
| G3 | `graceful_preset_stops_immediately` | interrupt before start → 0 applied, fast return, worker exits clean |
| G4 | `graceful_mid_feed_drains_delivered_not_more` | after first outcome, graceful → keep draining to worker exit; delivered == applied; unsent items never fed |
| G5 | `force_mid_inflight_discards_and_exits` | force mid-item → that item not applied, no hang, delivered applied, wait bounded |
| G6 | `panicked_worker_terminates_loop` | worker dies without `None` → loop returns via `is_finished`, earlier outcomes still applied |
| G7 | `drain_to_empty_on_exit` | worker bursts > `drain_chunk` outcomes before exit → **all** applied (phase-3 loop) |
| G8 | `on_sent_receives_only_handed_items` | `on_sent` == actually-sent items, not channel-full leftovers |
| G9 | `tick_false_stops_feed` | tick → `false` stops feeding, drains, exits |
| G10 | `exit_marker_bookkeeping` | two workers, both send `None` → terminated via `exited == len` |

## What stays phase-side (NOT superseded — do not delete)

- **Worker-contract tests** per phase (`*_worker_sends_none_*`, force-mid-file behavior,
  outcome content) — those test the worker, not the loop.
- **apply / ingest DB tests** — flags + phase + error-log per outcome.
- **`run()` endgame + interrupt + resume tests** — mismatch, fail_fast, skip, pending-zero,
  interrupt tails, queue drop-vs-survive, resume-completes.

## Tests to DROP as superseded by G1–G10 (user-approved)

- sparsify: `loop_exit_via_dequeued_eq_feed_total`, `loop_exit_via_graceful_preset_stops_feed`,
  `loop_applies_partial_batch_no_loss`, `loop_panicked_worker_gets_no_special_treatment`.
- dedup: `loop_exit_via_dequeued_eq_feed_total`, `loop_exit_via_exited_threads_and_one_running`.
- hash: `final_drain_applies_partial_batch_no_loss`, `interrupt_mid_feed_pauses_and_resume_completes`,
  `interrupt_dequeue_only_finishes_in_flight`, `triple_interrupt_force_discards_in_flight`,
  `interrupt_during_final_drain_applies_all`, `run_force_before_start_returns_interrupted`.
- Each phase keeps a **thin smoke** (an existing `run_*` success/interrupt test) proving the
  shared loop completes inside the real phase.

## Build / test commands

- `cargo build -p tar-dedup -p tar-dedup-cli` (green; no new warnings from touched files).
- `cargo test -p tar-dedup --lib` (dev profile only — never `--release`; `panic="abort"`
  in release breaks `catch_unwind`).
- Targeted: `cargo test -p tar-dedup --lib -t 'common::tests::G-suite'`.

## Gotchas for the implementer

- The dialect captures closures by reference/mutably like the current `apply_chunk`/
  `drain_chunk` closures — keep counters in outer scope, read them after the call.
- `recv.is_empty()` (crossbeam) is stable only after all workers are joined (no live
  senders); the join→drain ordering in Phase 3 is what makes it deterministic.
- Worker handles are moved into the loop; `at_least_one_running(&handles.iter().collect())`
  needs a `&Vec<&JoinHandle>` layout (as today).
- dedup MUST keep `apply_on_partial=true` and the drain→tick→feed order (FSM).
- hash/rehash `run()` currently pass `is_running`; delete that closure and hand over
  `thread_handles` by value.
- Panicked worker: `join()` returns `Err` — ignore (`let _ =`). A panic during
  `out.send` inside a worker is a worker-thread panic only; the loop must still return.

## Follow-ups (recorded, NOT this round)

- **Stage 3**: extract `place` ("materialize") loops — turn the batched materialize phase
  into a channel pipeline and adopt `send_receive_loop`.
- `plans/rehash-tests.md` (Stage 2): rehash test suite, loop-contract parts rewritten
  against the shared loop.
- hash's force `> DRAIN_CHUNK` loss and the no-join race are fixed for all phases by this
  plan (drain-to-empty + join) — no separate follow-up needed.
- "As built" section to be filled at implementation time (bugs flushed out + fixes).