# Plan: Sparsify-phase rewrite (queue + channel loop, hash-style) + unit tests

Status: **settled with the user (2026-09-25) — implementation round.** Production redesign is
complete; this round implements (a) a few production corrections agreed in review, (b) the unit
test suites for `db/sparsify.rs` and `archive/sparsify.rs`. Tests live inline in `#[cfg(test)]
mod tests` at the bottom of the two files (no `tests/` additions).

## Production corrections (this round)

### P1. Dropped `phase` from the shared candidate predicate
`sparsify_candidates_where()` no longer filters `phase = 'deduped'`. The candidates set is the
**overall workload** for the phase (stable across sessions), not "remaining this session":

```
exclude      = rows matched by promote_non_sparsify_candidates_to_sparsified  (keeps its own
               `phase = 'deduped'`; unchanged)
candidates   = NOT exclude  → property-only predicate, any phase:
               canonical_id = id AND ftype = 'file' AND sparse_count >= :min_pages AND {archive filter}
todo         = candidates AND phase = 'deduped' AND (flags &:has_sparse) = 0 AND (flags &:error_flag) = 0
done         = candidates AND phase = 'sparsified' AND (flags & (has_sparse | error_flag)) != 0
```

Rationale (user): on resume the phase bar must show `count_all` (workload) + position
(`= count_all − todo`) so the user sees previous-session progress; a bar that only counts
remaining rows reads as "no progress" after a stop/restart.

`sparsify_candidates_where()` params: `:min_pages` only.

### P2. `count_all_sparsify_candidates(min_pages)` (new)
`SELECT COUNT(*) FROM files WHERE {sparsify_candidates_where()}` — the workload, stable across
sessions. Mirrors `db/hash.rs::count_all_hashable_files`.

### P3. `count_pending_sparsify_candidates` = `todo`
`{sparsify_candidates_where()} AND phase = 'deduped' AND (flags &:has_sparse) = 0
AND (flags &:error_flag) = 0` — exactly the un-limited `pull_pending_sparsify_rows`, giving
bar-total/pull parity. Both flag bindings live here (they were dead/leftover before).

### P4. `run()` preamble — hash/dedup style workload/position
```rust
let workload = rt.db.count_all_sparsify_candidates(min_pages)?;
let pending  = rt.db.count_pending_sparsify_candidates(min_pages)?;
rt.progress.set_phase_total(workload);
rt.progress.set_phase_position(workload.saturating_sub(pending)); // = done
if pending == 0 { drop_sparsify_queue(); sanity_no_deduped(); return Ok(()); }
```
`done ⟺ workload − todo` holds because ingest sets flag + phase atomically.

### P5. Dead `:has_sparse` bindings removed
`promote_non_sparsify_candidates_to_sparsified`, `populate_sparsify_queue` no longer bind
`:has_sparse` (their SQL doesn't reference it → rusqlite `InvalidParameterName` at runtime).
`pull_pending_sparsify_rows` keeps its `:has_sparse` filter (resume skip of already-sparsified).

### P6. Drain tail (already applied by user)
`run_enqueue_dequeue_loop_sparsify` drain tail: `dequeue_total == feed_total` (hash parity, not
`feed_total + io_jobs` — the latter is unreachable since `dequeue_total ≤ feed_total`). Also the
feed loop's exit joins `feed_exhausted && feed_idx == feed_buf.len()` with
`!at_least_one_running(...)` like hash.

P1–P5 apply to `db/sparsify.rs` + `db.rs` facades + `archive/sparsify.rs::run`. P6 already in
the tree.

## db/sparsify.rs test module

Raw `Connection` + `schema::initialize`, no TestWorld (mirrors `db/dedup.rs`/`db/hash.rs`).
Fixture: `open_db()` (tempdir + schema + `filter_reason_archive` -1 internal include rule FK
seed), `insert_candidate(conn, id, size, sparse_count)` defaulting
`phase='deduped'`, `ftype='file'`, `canonical_id=id`, `sha1=[7;20]`, `include_reason_archive=-1`,
`exclude_reason_archive=0`. `min_pages = 4` default.

1. `promote_covers_every_or_disjunct_and_keeps_candidate` — one row per promote OR arm
   (`canonical_id IS NULL`, `canonical_id != id`, `ftype != 'file'`, `sha1 IS NULL`,
   `sparse_count IS NULL`, `sparse_count < min_pages`, `NOT(filter)`) + one keeper. Assert 7 →
   `sparsified`, keeper stays `deduped`.
2. `pull_skips_promoted_and_errored_rows` — create+populate queue; set one row `sparsified`,
   one `ErrorWhileSparsify`; `pull(0,100)` returns only the pending row; slice-walk by returned
   pos reproduces the full list.
3. `pull_filters_has_sparse_and_errorwhilesparse` — flag-level variant of #2: rows with
   `HasSparse` or `ErrorWhileSparsify` set (phase still `deduped`) are not listed.
4. `count_pending_matches_pull` — `count_pending` == un-limited pull length (todo parity),
   across a mix of pending/promoted/errored rows.
5. `count_all_is_workload_across_sessions` — seed 3 candidates; ingest 1 success →
   `count_all` stays 3, `count_pending` drops to 2 (`workload − pending == 1` == done);
   also after a *full* ingest `count_all` still 3, `count_pending` 0.
6. `ingest_flag_and_phase_are_atomic` — batch of `None`/`FileStat`/`Interrupted` outcomes;
   assert per row: `HasSparse ⟺ phase='sparsified'`, `ErrorWhileSparsify ⟺ phase='sparsified'`,
   Interrupted rows untouched (flags 0, phase `deduped`), resolved count correct.
7. `ingest_sets_modified_correctly` — `modified: true` on both None and FileStat arms sets
   `Modified`; `false` leaves it clear (incl. Interrupted row).
8. `ingest_panics_on_invalid_error_variants` — `Config`/`Other`/`Database` via `catch_unwind`
   (hash mirror).
9. `queue_populate_orders_by_size_desc` — 3 candidates sizes 8MiB/4MiB+1/1MiB →
   `pull::<StrippedRecord>(0,100)` yields ids size-DESC, positions monotone.
10. `populate_is_idempotent` — run twice → same full pull (INSERT OR IGNORE).

## archive/sparsify.rs test module

TestWorld mirrors `archive/dedup.rs` + `archive/hash.rs` **plus `work_dir` set**
(`dir/astage`) so `stage_dir()` is writable. Config overrides: `sparse { sparsify: true,
page_size: 4096, min_pages: 4 }`, `io_jobs: 2`, work_dir real. Payloads: all-zero vectors so
sparse rewrites are genuinely hole-y; ≤ 16 MiB, few per test.
Helpers: `add_zeros(name, n)` (write real file + `insert_file`), `phase(id)`, `flag(id, flag)`,
`seed_dedup_row(id, sparse_count)` (tx: `phase='deduped', canonical_id=id, sparse_count=:n,
sha1=[7;20]`), `stage_exists(id)` (sp.{content_id} under work dir), `prepare_for_sparsify()`
(`promote_non_sparsify_candidates_to_sparsified` + `create/populate_sparsify_queue`),
`run_loop(world, bar, work_cap)` spawning one worker + calling
`run_enqueue_dequeue_loop_sparsify` directly. `apply_no_filter` + `filter_reason -1` seed.

`sparse_one`:
1. `sparse_one_inaccessible_returns_filestat` — chmod 000 source (root-skip) →
   `Err(Error::FileStat(_))` with `io_path`.
2. `sparse_one_force_interrupts_mid_copy` — force preset → `Err(Error::Interrupted)`, dst
   absent (TempSparseFile drop-delete).

`sparsify_worker`:
3. `worker_sends_none_on_graceful_and_channel_close` — (a) row queued + graceful preset →
   only `None` on out; (b) drop `work_s` → worker exits with `None`.
4. `worker_panics_when_result_channel_closed` — close `out_s` → the `.expect` fires; worker
   `join()` returns Err (expects aren't silently swallowed).
5. `worker_success_outcome` — queued self-canonical candidate → `Some(outcome)` with `id`,
   `modified=false`, `err=None`; `sp.{content_id}` exists under stage_dir.
6. `worker_unreadable_records_error_outcome` — chmod 000 (root-skip) → `Some(outcome)` with
   `err: Some(FileStat(_))`, no panic.

runner loop (drives `run_enqueue_dequeue_loop_sparsify` directly, one real worker):
7. `loop_exit_via_dequeued_eq_feed_total` — 3 candidates, work_cap 2 → completed 3; drain tail
   exits on `dequeue_total == feed_total`; all rows flagged.
8. `loop_exit_via_graceful_preset_stops_feed` — graceful pre-set → completed 0, loop
   terminates via `is_interrupted()`/exit conditions.
9. `loop_applies_partial_batch_no_loss` — work_cap=2, 3 candidates → final override drain
   covers the ragged tail (completed 3).
10. `loop_panics_on_other_error_variant` — pre-push `Some(SparseOutcome{err:
    Some(Error::Config)})` into out channel; run loop on test thread → `catch_unwind` → panic.
11. `loop_panicked_worker_gets_no_special_treatment` — worker with closed `out_s` panics
    mid-run; loop still returns `Ok((completed, errored))`, no hang; the dropped in-flight row
    stays `deduped` (resume-able).

`run()`:
12. `run_disabled_promotes_all_no_flags` — `sparsify: false` → all `deduped→sparsified`, no
    HasSparse, queue dropped.
13. `run_errors_when_stage_uncreatable` — `work_dir` shadowed by a regular file →
    `Err(FileStat)` with `io_path == stage_dir` (no root needed).
14. `run_sparsifies_candidates_and_flags` — 3 all-zero candidates (sparse_count 8/4),
    1 sub-min, 1 dup-canonical → candidates `sparsified`+HasSparse with `sp.*` file present;
    non-candidates promoted w/o flag; no `deduped` rows left; queue gone (sanity passes).
15. `run_graceful_and_force_preset_return_interrupted` — both → `Err(Error::Interrupted)`;
    both leave flags 0 and the queue alive for resume.
16. `run_graceful_interrupt_mid_run_resume_completes` — several all-zero candidates; trigger
    thread on bar length → `1 <= completed < N`; resume with fresh `Shutdown::detached()` →
    all done + flagged; `count_all` stable across both runs, `count_pending` 0 after resume.
17. `run_force_interrupt_mid_run_discards_in_flight` — force mid-copy → in-flight row stays
    `deduped` (Interrupted outcome dropped), errored==0; resume completes it.
18. `unreadable_file_recorded_not_fatal` — chmod 000 candidate (root-skip) →
    `ErrorWhileSparsify` + `sparsified` + one error record (`ErrorPhase::Pipeline(Sparsify)`),
    siblings clean.
19. `modified_file_flagged` — stale mtime (`Utc::now()-3600`) → `Modified` set.
20. `record_sparsify_error_persists_filestat` — direct `record_sparsify_error` + flush →
    record has `file_id`, `error_type == "Io/PermissionDenied"`, phase `Pipeline(Sparsify)`.

## Verification
- `timeout 900 nix develop --command cargo build -p tar-dedup -p tar-dedup-cli` (prod green).
- `timeout 900 nix develop --command cargo test -p tar-dedup --lib -- 'sparsify'` (db+archive).
- `timeout 1500 nix develop --command cargo test -p tar-dedup --lib` — expect the 4 pre-existing
  failures (inventory ×2, scan ×2) + 1 ignored hash stub; nothing else.
- Payloads ≤ 16 MiB; never `--release`; no `cargo fmt`.

## Notes / decision log
- `sparsify_candidates_where()` = workload (no phase); `count_all`/`count_pending` split mirrors
  hash's `count_all_hashable_files`/`count_pending_hashable_files` and dedup's
  `count_dedup_phase_total`/`count_dedup_phase_position`.
- `done = workload − todo` relies on ingest's atomic flag+phase; no separate done count needed.
- `count_pending` keeps both `:has_sparse` and `:error_flag` (todo excludes both), matching pull.
- The `:has_sparse` filter stays ONLY in `pull_pending_sparsify_rows` (+ `count_pending` now);
  populate/promote must not bind it (P5).
- `io_jobs` worker count (I/O bound), not `effective_jobs()`.
- Result channel `bounded::<Option<SparseOutcome>>`; `None` = per-worker thread-exit marker.