# 090 — Dedup ingest: prepared statements + single-transaction FSM tick

Status: **implemented (2026-11-13)**; both §5 decisions took the recommended path.

Preceded by 089 (hash ingest). The dedup ingest itself is healthy: every write is
PK-addressed, there is no missing-index scan, and `ingest_compare_outcome` already
wraps one batch in one transaction (`db/dedup.rs:797,825`). The remaining cost is
per-row statement re-preparation and five separate transactions per FSM tick.

## Motivating observations

- `archive/dedup.rs:277-290` applies outcomes through
  `ingest_compare_outcome`; per outcome it calls `or_file_flags` (now cached),
  `set_canonical` **or** `set_file_flag`, then `unmark_inflight` — all re-prepare
  their SQL on every call.
- `archive/dedup.rs:252-276` `tick` calls five FSM functions, and each opens and
  commits its **own** transaction (`db/dedup.rs:232,264,308,348,409`). The tick
  runs every loop iteration, so that is ~5 commits per iteration.
- The five FSM queries are complex `(sha1,size) IN (SELECT …)` aggregates; they
  are prepared once per tick per function.

## 1. `prepare_cached` for the remaining per-row statements

| Function | File | Change |
|---|---|---|
| `set_file_flag` | `db/flags.rs:352` | `conn.execute` → `conn.prepare_cached(...)?.execute(...)` |
| `set_file_phase` | `db/common.rs:531` | same |
| `set_canonical` | `db/dedup.rs:562` | same |
| `mark_inflight` | `db/dedup.rs:537` | `conn.prepare` → `conn.prepare_cached` |
| `unmark_inflight` | `db/dedup.rs:551` | `conn.prepare` → `conn.prepare_cached` |

`or_file_flags` is already cached. The distinct cached SQL count stays ~11, under
rusqlite's default capacity of 16, so no capacity change is required for this
part. `set_file_flag`/`set_file_phase` also back scan / place / tar-builder loops.

No signatures change. Each `CachedStatement` is a temporary (dropped before the
enclosing transaction commits), so the per-connection cache borrow is never held
across `tx.commit()`.

## 2. Single-transaction FSM tick

1. Each transition becomes a private `fn name(conn: &Connection, eager_filter: bool)`
   that executes its statements directly. Outside an explicit transaction every
   SQLite statement is already its own implicit transaction, so a single-statement
   transition needs no `conn.transaction()`; the multi-statement ones rely on the
   caller's ambient transaction for atomicity.
2. The five `Database` methods are dropped. The only production entrypoint is the
   aggregate below, which owns the one transaction.
3. Add to `db/dedup.rs` (and re-export on `Database` in `db.rs`):

   ```rust
   pub struct DedupFsmProgress {
       pub groups_errored: u64,   // from finish_to_error
       pub files_promoted: u64,   // sum of all promotions across the tick
   }

   /// Advance the dedup group FSM atomically. Calls, in order:
   ///   1. searching_to_finished
   ///   2. finish_to_error
   ///   3. finish_to_done
   ///   4. finish_to_ready
   ///   5. ready_to_searching
   pub fn advance_dedup_fsm(conn: &mut Connection, eager_filter: bool)
       -> Result<DedupFsmProgress>
   ```

   One `conn.transaction()`, the five calls in that order, one commit. The doc
   comment lists the sequence explicitly (requested).
4. `archive/dedup.rs:252-276` `tick` becomes one `advance_dedup_fsm` call:
   `groups_errored` drives the `fail_fast` branch, `files_promoted` drives
   `inc_both`, and `count_pending_dedup_groups() == 0` stays outside the wrapper
   as the loop-stop check.

Behavioral note: a tick is now atomic. A panic mid-tick rolls back the whole
tick instead of persisting earlier transitions; a resume sees one consistent FSM
state.

## 3. Existing progress-accounting bug (decision §5.1)

`archive/dedup.rs:269-271`:

```rust
if promoted1 + promoted2 + promoted3 > 0 {
    rt.progress.inc_both(promoted);   // `promoted` is finish_to_error's count
}
```

The guard tests `promoted1+promoted2+promoted3` but increments by the
`finish_to_error` count (and that count was already added at `:255-257`). With
`files_promoted` aggregated in the wrapper, the natural implementation adds the
correct total. Recommended: fix it.

## 4. Out of scope

- Reworking the five FSM aggregates into a single scan.
- Changing `apply_on_partial` / the drain/feed coupling.
- The wide-row `files` rewrite cost (schema split).

## 5. Decisions taken

1. **Progress-count bug (§3): fixed.** The tick now adds the aggregate
   `files_promoted` once. Before, `finish_to_error`'s count was added twice (once
   directly, once via the buggy second `inc_both`) and the other three
   transitions were never counted.
2. **FSM SQL caching: deferred.** The five helpers still prepare their statements
   per tick; the transaction wrapper landed first. Revisit after measuring.

## 6. Verification

- Existing FSM tests: `ready_to_searching_elects_min_and_flips`,
  `ready_to_searching_two_member_group`,
  `ready_to_searching_no_electable_marks_done_unreachable_guard`,
  `searching_to_finished_fires_when_all_resolved`,
  `searching_to_finished_requires_exactly_one_self_canonical`,
  `finish_to_done_promotes_and_all_have_canonical`,
  `finish_to_error_errored_with_all_errored_candidates`,
  `finish_to_ready_retires_canonical_and_clears_flags`,
  `fsm_roundtrip_two_member_unequal_terminates_done`,
  `fsm_roundtrip_three_member_unequal_than_equal_terminates_done`.
- Existing ingest tests (`ingest_compare_outcome_*`).
- `cargo test -p tar-dedup-lib --lib`, then `scripts/vm-test.sh` (`test-all`).
- Result (2026-11-13): `--lib` **770 passed**; dedup subset **55 passed**.
  `--lib --features test-all`: **811 passed, 6 failed** — all six fail on the
  host's missing SELinux context (`file system does not support SELinux`), as
  expected. The VM was unreachable (`No route to host`), so the full 817-test
  run still needs to be done there.
- No new tests unless requested (the five keep their own tests and are now
  called directly with `&Connection`; the wrapper composes them).

## 7. Change map

| File | Change |
|---|---|
| `db/flags.rs` | `set_file_flag` cached |
| `db/common.rs` | `set_file_phase` cached |
| `db/dedup.rs` | cached setters; five private `&Connection` transitions; `advance_dedup_fsm` |
| `db.rs` | dropped the five `Database` transition wrappers; kept `advance_dedup_fsm` |
| `archive/dedup.rs` | `tick` uses `advance_dedup_fsm`; progress/fail-fast from the aggregate |
