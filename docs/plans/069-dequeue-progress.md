# 069 — Dequeue progress: early per-pass counter increments

Status: **agreed (2026-11-06), implementation.** Fixes the long "no progress
during a batch phase" stall at the start of hashing (and every other
`apply_on_partial = false` phase).

## Problem

`DRAIN_CHUNK = 4096` (`common/defaults.rs:45`). In `send_receive_loop`
(`common.rs:237`) the drain holds outcomes in `pending_out` and only calls
`apply()` once 4096 accumulate (or when `drain_override`). Every batch phase
advances the progress counters *inside* `apply`, i.e. only when the DB write
happens — so the bar stands still, then jumps +4096 at once. On a fresh run
nothing is visible until the first 4096 files are hashed.

## Design — `on_dequeue(u64)`, once per drain pass, outside the while loop

New last parameter on `send_receive_loop` (`common.rs:210`):

```rust
on_dequeue: impl FnMut(u64)
```

In the drain closure, snapshot `*dequeue`, run the while loop, then call
`on_dequeue` once with the number of outcomes pulled this pass, **before**
`apply`:

```rust
let pulled_before = *dequeue;
while pending_out.len() < drain_chunk {
    match recv.try_recv() {
        Ok(Some(op)) => { pending_out.push(op); *is_busy = true; *dequeue += 1; }
        Ok(None)     => *exit += 1,
        Err(_)       => break,
    }
}
let pulled = *dequeue - pulled_before;
if pulled > 0 {
    on_dequeue(pulled);
}
if pending_out.len() >= drain_chunk || drain_override {
    apply(&mut pending_out)?;
    *is_busy = true;
}
```

Rationale (vs. incrementing inside the while loop): the drain loop exits fast
(threshold hit or channel empty), so per-outcome calls buy nothing renderable —
the position is only read at render time. One batched add per drain pass is
visually identical and far cheaper (one atomic add instead of N). It still fixes
the start-stall: early passes pull only the trickle that has arrived (1–2
outcomes), so the bar moves immediately, independent of the 4096 threshold.

### Aggregate correctness

Sum of `on_dequeue` args across all passes == number of dequeued outcomes ==
`dequeue_total` (the loop's return value) == what `apply` used to count. Every
dequeued outcome is always applied (phase-2/3 drain-to-empty), so the final bar
position is byte-identical to today — the change is purely *when* the counter
ticks (per drain pass instead of per-4096 DB commit). Increments happen before
the DB write, which is the intended UI posture (they reflect completed work,
not yet-committed work).

## Per-caller wiring (each passes its own inc; `apply` drops its old inc)

| Call site | `on_dequeue` | removed from `apply` |
|---|---|---|
| `archive/hash.rs:168`   | `\|n\| rt.progress.inc_both(n)` | `inc_both(n)` (+ `let n`) |
| `archive/sparsify.rs:214` | `\|n\| rt.progress.inc_both(n)` | `inc_both(n)` (+ `let n`) |
| `unarchive/rehash.rs:201` | `\|n\| rt.progress.inc_both(n)` | `inc_both(n)` (+ `let n`) |
| `unarchive/place.rs:215` (copy/move) | `\|n\| moved_bar.inc(n)` | `moved_bar.inc(n)` (ingest ret ignored) |
| `unarchive/place.rs:501` (materialize) | `\|n\| rt.progress.inc_both(n)` | `inc_both(n)` (+ `let n`) |
| `archive/dedup.rs:292` | `\|_\| ()` (no-op) | — (untouched) |

`dedup` stays as-is: it runs `apply_on_partial = true` so its FSM moves
continuously already, and its counting is via `ingest_compare_outcome` return +
per-group `tick` promotions (not 1:1 with dequeued outcomes), so a dequeue hook
would double-count.

## Tests (`common.rs` toy harness)

- Extend `run_toy` (`:365`) with an `on_dequeue: impl FnMut(u64)` parameter;
  pass `|_| {}` at the existing ~8 call sites.
- New `on_dequeue_sums_to_dequeued_outcomes`: 2 workers, 8 items, `drain_chunk
  = 2`; assert `on_dequeue` counts sum == `dequeue_total` == applied count, and
  each individual call is `> 0 && <= drain_chunk` (batched per pass, never
  per-outcome, never over-threshold).

Phase suites (hash/sparsify/rehash/place) need no changes — aggregate-identical
flow. Verify with the full `--features test-all` suite on the VM.

## Files touched

- `crates/tar-dedup-lib/src/common.rs` (signature + drain + toy tests)
- `crates/tar-dedup-lib/src/archive/hash.rs`
- `crates/tar-dedup-lib/src/archive/sparsify.rs`
- `crates/tar-dedup-lib/src/archive/dedup.rs` (no-op closure)
- `crates/tar-dedup-lib/src/unarchive/rehash.rs`
- `crates/tar-dedup-lib/src/unarchive/place.rs` (two call sites)