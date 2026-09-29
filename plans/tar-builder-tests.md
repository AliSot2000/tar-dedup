# Plan: tar_builder unit tests + prerequisite production changes

Status: **decisions settled with the user (2026-09-28); implementation follows.**
Add a unit-test module to `archive/tar_builder.rs` (none today) plus db-level
tests in `db/tar_writer.rs`, covering every function in the archive phase: the
util helpers, session lifecycle (recover / force-abort / append_snapshot /
end_session), and the full `run()` contract (finish, graceful/force interrupt,
resume, canonicalize failure, append_path flag handling). Requires two small
production changes first: fix the force-interrupt path in `run()`'s loop, and
replace the single-shot `list_staged_canonical_ordered` pull with a batched,
temp-table-ordered queue (sort flag hard-coded `false` for now).

## Decisions (user-confirmed)

1. **Fix `run()` loop to break on `is_interrupted()`** (not just `is_graceful()`)
   so force interrupts route through `force_abort_session` instead of panicking.
2. **Include the production changes as the first steps** of the test plan.
3. **Skip direct `TarWriter` unit tests** this round (EOF hold-back, stream
   shapes) — they are exercised through the run/end-session tests.
4. **Sort flag is a DB-function argument only, hard-coded `false`** at the call
   site. No CLI arg, no config field (so no `ArchivePipelineOptions` literal
   churn in the four `test_archive_config()` copies). CLI routing is deferred.

## Part 1 — Production changes (prerequisite)

### 1.1 `archive/tar_builder.rs::run` — interrupt routing fix

Current bug: the loop breaks only on `rt.shutdown.is_graceful()` (tar_builder.rs:81).
On a force interrupt `is_graceful()` is false, so the loop keeps appending until
the encoder's `check_in_flight()` aborts mid-buffer-flush → `append_path` returns
`Error::Interrupted` → falls into the `Err(e) => panic!("Unexpected return type")`
arm. `force_abort_session` (line 140) is therefore unreachable during append.

Change:
- Loop condition → `if rt.shutdown.is_interrupted() { stopped = true; final_archive = false; break; }`.
- Add an `Err(Error::Interrupted)` arm in the `append_path` match:
  `Err(Error::Interrupted) => { stopped = true; final_archive = false; break; }`
  (member may be partially written; recovery truncates the session on resume).
- Keep `Err(e @ Error::FileStat(_))` (soft-continue) and the `Err(e) => panic!`
  catch-all (now truly unreachable for interrupts).

### 1.2 `db/tar_writer.rs` — batched ordering queue (mirror `sparsify_queue`)

Replace the single-shot `list_staged_canonical_ordered` with a temp-table queue
that stores the ordering, pulled in batches via `batched_stepped_loop`.

New functions (all take `conn`, plus `sort_by_name: bool` where relevant):

- `create_archive_queue(conn) -> Result<()>` — idempotent:
  `CREATE TABLE IF NOT EXISTS archive_queue (
      id INTEGER PRIMARY KEY,
      file_id INTEGER NOT NULL UNIQUE REFERENCES files(id) )`
- `populate_archive_queue(conn, sort_by_name: bool) -> Result<u64>` — idempotent
  (`INSERT OR IGNORE` via `UNIQUE(file_id)`), row_number as `id`:
  - `sort_by_name == false`: `ORDER BY ext ASC, size ASC, id ASC`
  - `sort_by_name == true`: `ORDER BY ext ASC, <basename(abs_path)> ASC, size ASC, id ASC`
    where basename uses the SQLite idiom
    `replace(abs_path, rtrim(abs_path, replace(abs_path, '/', '')), '')`
    (verified: strips the directory prefix, leaving the trailing component).
  - WHERE clause = the staged-canonical predicate (canonical_id = id, phase =
    'staged', sha1 IS NOT NULL, ftype = 'file', `generate_archive_filter(None)`).
- `pull_pending_archive_rows<R: SqlFileRow>(conn, index: u64, limit: u64)
  -> Result<Vec<(u64, R)>>` — join `files` on `archive_queue.file_id`, filter
  `archive_queue.id > :index` AND still-pending rows:
  `files.phase = 'staged' AND (files.flags & AppendedPath) = 0 AND
   (files.flags & ErrorWhileArchive) = 0`, `ORDER BY archive_queue.id LIMIT :limit`.
  Returns `(queue_pos, R)` so the cursor advances by returned pos (same contract
  as `pull_pending_sparsify_rows`).
- `drop_archive_queue(conn) -> Result<()>` — idempotent `DROP TABLE IF EXISTS`.
  Called on phase success only (like sparsify); left across interrupts so a
  resume repopulate is a no-op for already-queued rows.

Facade additions in `db.rs`: `create_archive_queue`, `populate_archive_queue`,
`pull_pending_archive_rows`, `drop_archive_queue`. `list_staged_canonical_ordered`
stays (used by nothing else after the rewrite; keep for now or fold callers).

### 1.3 `archive/tar_builder.rs::run` — batched loop

Replace the `for file_id in to_archive` block with:

```rust
rt.db.create_archive_queue()?;
rt.db.populate_archive_queue(false)?;   // sort_by_name hard-coded false for now

batched_stepped_loop(
    BATCH_SIZE,
    || 0u64,
    |index: &u64, batch_size| rt.db.pull_pending_archive_rows::<StrippedRecord>(*index, batch_size),
    |(pos, _): &(u64, StrippedRecord)| *pos,
    |batch: Vec<(u64, StrippedRecord)>| { /* per-record body from current loop */ }
)?;
rt.db.drop_archive_queue()?;
```

`BATCH_SIZE = 10_000` (const, matching stage/sparsify). The per-record body keeps
the existing logic (graceful/force break, canonicalize, `warn_if_times_changed`,
`append_path`, flag setting), but `stopped`/`final_archive` must be captured
across the closure — use a `&mut` captured in the closure (`let mut stopped =
false; let mut final_archive = true;` moved into the closure via `|batch| { ... }`
and read back after `batched_stepped_loop` returns). The `break` statements become
`stopped = true; final_archive = false; return Ok(());` (early return from the
closure) — but the loop must distinguish "graceful stop" (finish session) from
"force stop" (abort). Since `batched_stepped_loop` propagates `Err`, use:
- graceful/force detected mid-batch → set `stopped`, return `Ok(())` from the
  closure; after the loop, existing `if stopped && is_force() → force_abort_session`
  and `if stopped → end_session(...); return Err(Interrupted)` logic runs.

Drop the queue on the success path only (after `end_session` returns Ok), mirroring
sparsify's `drop_sparsify_queue` placement. On abort paths the queue persists for
resume.

## Part 2 — Tests in `archive/tar_builder.rs` (`#[cfg(test)] mod tests`)

Copy the `TestWorld`/`test_archive_config()` pattern from `archive/stage.rs`
(io_jobs: 1, `sparse.sparsify: false`, `compression.format: None` for speed,
`pipeline.write_archive_footer: false` unless a test flips it). Add helpers:
- `seed_staged_row(id)`: file via `add_file` + `UPDATE files SET phase='staged',
  canonical_id=:id, sha1=..., include_reason_archive=-1, exclude_reason_archive=0`
  + `place_symlink(id)` (symlink at `stage_dir/tar_member_name` → `abs_path`).
- `seed_open_session(offset)` / `seed_finalized_session(offset)` via raw SQL on
  `archive_sessions` (or the facade where possible).
- `phase(id)`, `flag(id, flag)`, `errors_for(id)` (via `get_records_by_file_id`),
  `bytes_in()/bytes_out()` (via `get_archive_bytes_in/out`), `session_finalized()`.

### 2.1 Util-level tests

- `archive_file_len_missing_is_zero` — nonexistent path → 0.
- `archive_file_len_returns_len` — write N bytes → N.
- `truncate_archive_at_nonexistent_is_ok` — no file → Ok, no file created.
- `truncate_archive_at_truncates_to_offset` — write 1 MiB, truncate at 4096 →
  len == 4096, file still exists.
- `truncate_archive_at_zero_removes_file` — write bytes, truncate at 0 → file gone.
- `check_archive_bytes_out_*` (4 outcomes):
  1. no finalized session → Ok (any archive_len).
  2. finalized session + `bytes_out` None → Ok.
  3. finalized session + `bytes_out == archive_len` → Ok.
  4. finalized session + `bytes_out != archive_len` → `catch_unwind` returns Err
     (assert_ne panic).

### 2.2 Session-level tests

- `recover_incomplete_session_without_open_session_is_ok` — no open session row →
  Ok, archive untouched.
- `recover_incomplete_session_truncate_error_aborts` — seed an open session row
  (offset e.g. 100), point `archive_path` at a directory (or otherwise make
  `OpenOptions::write().open` fail) → `run()` returns `Err(FileStat)` whose
  `io_path()` is the archive path; a session-scoped error row exists
  (`get_records_by_file_id` won't cover it — use `db.list_records(ErrorScope::Session, ...)`
  or the `count_records` facade with Session scope).
- `force_abort_session_keeps_session_open_and_no_bytes` — seed staged rows +
  open session, preset `shutdown.request_force()`, `run()` → `Err(Interrupted)`;
  assert: session row still `finalized = 0`; `bytes_in`/`bytes_out` meta unchanged
  (None); no file promoted to `archived` (all still `staged`, `AppendedPath` unset);
  archive file has no finalizing bytes appended (len == pre-run len, or just that
  no tar member was written given the loop breaks before appending).
- `append_snapshot_uses_init_vs_regular_name` — call `append_snapshot(&mut writer,
  rt, true, rec)` then a fresh writer with `false`; assert the tar contains
  `manifest.sqlite` (init) / `snapshot.sqlite` (regular) as member names.
- `append_snapshot_overwrites_and_removes_staging` — pre-create
  `stage_archive_snapshot()` path with junk; run `append_snapshot`; assert the
  staging file is gone afterwards (overwritten then removed).
- `append_snapshot_ignores_inflight_abort` — preset `shutdown.request_force()`;
  `append_snapshot` still completes Ok (no `check_in_flight` inside it).
- `end_session_promotes_pending_and_requires_empty` — seed staged rows with
  `AppendedPath` set; `end_session(write_tar_eof=true)` promotes them to
  `archived`; separately, with a leftover `staged` row present, `catch_unwind`
  the `assert_eq!(0, count_files_in_phase(Staged))` panic.
- `end_session_stamps_finalizes_and_writes_footer` — `write_archive_footer: true`;
  run through `run()`; assert `has_valid_footer(archive)` is true, session
  `finalized = 1` with `finished_at` set, `bytes_in`/`bytes_out` meta set and
  `bytes_out == archive_file_len`.
- `end_session_graceful_interrupt_finalizes` — graceful stop mid-run → session
  finalized (stream closed, `finalize_session` path), `bytes_in/out` set; run
  returns `Err(Interrupted)`.

### 2.3 `run()`-level tests

- `run_finishes_archive` — seed N staged rows + symlinks, `run()` → Ok; all rows
  `Archived`; archive exists with N payload members (+ manifest + snapshot);
  `AppendedPath` set on all; `bytes_out` matches file len.
- `run_graceful_interrupt_then_resume` — preset `request_graceful()`, `run()` →
  `Err(Interrupted)`, session finalized, already-appended rows `Archived`; then a
  fresh `run()` (no interrupt) completes the rest; assert total archived == N and
  no duplicate members (already-archived rows not re-appended).
- `run_force_interrupt_then_resume` — preset `request_force()`, `run()` →
  `Err(Interrupted)` via `force_abort_session` (session open, no bytes meta, no
  rows promoted); then `run()` again → `recover_incomplete_session` truncates the
  archive and clears pending; run completes; assert all N archived, archive valid.
- `run_canonicalize_failure_marks_or_raises` — two variants:
  - soft: break the staged symlink (remove it) → `ErrorWhileArchive` flag set,
    member skipped, run continues to completion.
  - `fail_fast: true` → `run()` returns `Err(FileStat)`.
- `run_append_path_success_sets_flags` — success path sets `AppendedPath`; with a
  stale recorded mtime, also sets `Modified`.
- `run_append_path_filestat_error_marks_and_continues` — make `append_path` fail
  with a FileStat error (e.g. staged symlink target unreadable via chmod 000,
  root-guarded) → `ErrorWhileArchive` set, `AppendedPath` NOT set, run completes.
- `run_append_path_other_error_panics` — inject a non-FileStat error (e.g. a
  `CompressionFormat::None` writer whose `append_path` returns `Error::Other` via
  a stub, or by pointing the archive at an unwritable path such that
  `TarWriter::open` fails → that's a FileStat; to force `Error::Other` use a
  `tar_name` that makes `append_path_with_name` fail with a non-io error — if not
  reachable, assert the `Err(e) => panic!` arm via `catch_unwind` on a crafted
  `Error::Other` returned from a stub). If truly unreachable, note it and assert
  only that Interrupted is handled (not a panic).

## Part 3 — Tests in `db/tar_writer.rs` (`mod tests`, schema::initialize)

- `abort_incomplete_session_resets_pending_to_staged` — seed an open session row;
  rows: (a) `AppendedPath` + phase `staged`; (b) `AppendedPath` + phase `archived`;
  (c) plain `staged`; (d) `archived` without flag. `abort_incomplete_session` →
  (a) reset to `staged` + flag unset; (b) untouched (already archived, flag kept);
  (c)/(d) untouched. Also assert the session row now `finalized = 2` (ABORTED).
- `clear_archive_session_pending_only_non_archived` — same matrix, call
  `clear_archive_session_pending` directly.
- `create_populate_pull_archive_queue_slices` — seed staged canonicals, populate
  both `sort_by_name` variants, walk `pull_pending_archive_rows` with index/limit,
  assert full coverage, no overlap, correct order (ext, size, id vs ext, name,
  size, id), and that already-`AppendedPath` or `ErrorWhileArchive` rows are
  excluded.
- `populate_archive_queue_is_idempotent` — populate twice → same row set (no dupes).
- `drop_archive_queue_is_idempotent` — drop twice → Ok.
- `reset_archive_state_wipes_sessions_and_resets` — rows with `AppendedPath` /
  `archived` reset to `staged`, flag cleared; `archive_sessions` emptied.

## Files touched

1. `crates/tar-dedup/src/archive/tar_builder.rs` — interrupt fix, batched loop,
   new `mod tests` (~14 tests).
2. `crates/tar-dedup/src/db/tar_writer.rs` — queue functions + `mod tests`
   (~6 tests).
3. `crates/tar-dedup/src/db.rs` — facades for the queue functions.

## Notes / gotchas

- `Error::io` maps `ErrorKind::Interrupted` → `Error::Interrupted`; `append_path`
  surfaces encoder aborts that way — the new match arm is what makes force clean.
- `force_abort_session` calls `writer.abandon()` (drops the compression stream
  without footer), `db.checkpoint()`, `progress.abandon()`, returns
  `Err(Interrupted)`. Test asserts the DB/archive side-effects only.
- `append_snapshot` calls `db.checkpoint()` then `fs::copy(db_path →
  stage_archive_snapshot)` then `writer.append_path`, then removes the staging
  file. It deliberately has no `check_in_flight` — finishing the snapshot retains
  the resume point (user judgement: correct).
- `end_session` on graceful stop uses `finalize_session` (no tar EOF); on full
  archive uses `finalize_archive` (tar EOF). Footer only when
  `write_tar_eof && write_archive_footer`.
- The `assert_eq!(0, count_files_in_phase(Staged))` in `end_session` fires only
  when `write_tar_eof` (full archive); catch_unwind it.
- Baseline: lib suite currently 211 passed / 4 pre-existing failures (inventory
  ×2, scan ×2) / 1 ignored. Expect ~211 + ~20 new = ~231 passed after this work.
- No `cargo fmt`, no `--release`.

## Verification

- `cargo build -p tar-dedup -p tar-dedup-cli` clean.
- `cargo test -p tar-dedup --lib -- tar_builder` and `-- tar_writer` iterate fast.
- Full `cargo test -p tar-dedup --lib` → same 4 pre-existing failures only.
- Manual smoke: `archive --fresh` still produces a valid archive with footer
  (regression check for the batched loop).