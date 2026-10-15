# Plan: Unit tests for the `stage` phase

Status: **implemented & verified (2026-09-28)** — tests written and passing.
Add a test module to `archive/stage.rs` (currently none) plus one db-level test in
`db/stage.rs`, covering the stage `run()` contract: structural-abort on stage-dir
creation failure, no-op when nothing is pending, resume/progress retention,
mtime-change detection, stale-target removal (success + failure), symlink-failure
recover-ability without reset, and a dozen-file end-to-end DB correctness check.

## Context (verified)

- `archive/stage.rs` uses `batched_stepped_loop` over `list_files_to_stage_after`
  (id-cursor, 10_000 batches), rayon pool of `io_jobs` workers, `(FileId, bool
  modified)` results. On error: records a persistent error row
  (`Recorder.record_file(id, ERROR_PHASE_STAGE, fse, …)`) and `return Err(…)`,
  never setting a stage error flag. Failing file keeps `phase='sparsified'`, so a
  re-run after the problem is fixed re-lists and re-stages it.
- `db/stage.rs` has `promote_unstageable_files` (**no db unit test yet**),
  `list_files_to_stage_after`, `count_all_stage_candidates`.
- Test infra precedent: `archive/{sparsify,hash,dedup}.rs` each carry a private
  `TestWorld` (tempdir + `Database::open` + seeded `filter_reason_archive` -1
  row + `test_archive_config()`), db tests use `schema::initialize`. Replicate
  per-module (no shared helper exists).
- Test config uses `io_jobs: 1` → rayon pool single-threaded, sequential batch
  order; `try_for_each` short-circuits on the first error, so a failing file
  stops later work in the batch (strict tail assertion is deterministic).
- Error-path flushing relies on `Recorder::Drop` best-effort flush (run returns
  `Err` before the explicit `flush()`); verified that path persists rows.

## Tests

### A. `archive/stage.rs` — new `#[cfg(test)] mod tests`

Copy `TestWorld` / `test_archive_config()` from `sparsify.rs` (io_jobs: 1), add:

- `seed_stage_row(id)`: file from `add_file`, then
  `UPDATE files SET phase='sparsified', canonical_id=:id, sha1=:s, include_reason_archive=-1, exclude_reason_archive=0`
- `stage_path(id)`: `work_dir.join(tar_member_name())`
- errors query via `db.get_records_by_file_id(id)`

1. `run_errors_when_stage_dir_uncreatable` — replace work_dir with a regular
   file so `create_dir_all` fails → `Err(FileStat)`; file stays `Sparsified`.
2. `run_with_no_pending_returns_ok` — rows already `staged` / none eligible →
   `run` returns `Ok`, nothing staged, work_dir has no new symlinks.
3. `run_resumes_partial_without_redoing_work` — 2 pre-staged (with real
   symlinks) + 4 sparsified → `run` → all 6 `Staged`; the 2 pre-staged symlink
   targets byte-identical after (`read_link`); 0 sparsified; no error rows.
   Then `run` again → `Ok`, idempotent.
4. `run_flags_modified_when_times_changed` — `add_file_recorded` with
   hour-stale mtime → `run` → `FileFlag::Modified` set, phase `Staged`.
5a. `run_removes_stale_target_and_stages` — pre-create a regular file at the
    target path → `run` → removed, correct symlink created, phase `Staged`, no
    error rows.
5b. `run_target_removal_failure_logs_and_aborts` — **strict**: pre-create a
    *directory* at a middle file's target (unlink on a dir errors) → `run`
    returns `Err(FileStat)`; error row recorded (`file_id`, phase `Stage`);
    failing file stays `Sparsified`; files after the failing one in id order
    stay `Sparsified` (never staged, short-circuit). Under `io_jobs: 1` the
    single-threaded rayon walk is id-ordered, so the strict tail assertion is
    deterministic.
6. `run_symlink_failure_fixes_and_reruns_without_reset` — chmod work_dir 0555
   (guard `if geteuid().is_root() { return; }` per existing precedent) → `run`
   returns `Err(FileStat)`, error row recorded, file stays `Sparsified`, **no**
   error flag, no symlink. chmod 0755 back, `run` again → `Ok`, file `Staged`,
   symlink present.
7. `run_stages_dozen_files_and_updates_db` — 12 eligible files → `run` → all 12
   `Staged` with symlinks, `count_files_in_phase(Sparsified) == 0`, 0 error rows.

### B. `db/stage.rs` — db-level "check again" test

8. `promote_unstageable_files_promotes_ineligible_arms` — seed `sparsified`
   rows that are: `ftype='dir'`, `canonical_id IS NULL`, `canonical_id != id`,
   `sha1 IS NULL` (+ one valid row) → all 4 ineligible promoted to `staged`,
   the valid row stays `sparsified`; `count_all_stage_candidates` stays 1.

## Notes / decisions

- Leave the existing `// TODO fix this up, this is not correct.` comment above
  the `db/stage.rs` test module untouched (user-confirmed).
- Strict tail assertion for 5b (user-confirmed).
- Assert `records[0].file_id` + `.phase == ErrorPhase::Pipeline(PipelinePhase::Stage)`
  and `len >= 1`, not platform-dependent `error_type` strings.

## Verification

- `cargo build -p tar-dedup -p tar-dedup-cli` — clean
- `cargo test -p tar-dedup --lib -- "stage"` — 15 passed (4 db::stage + 8 archive::stage
  + sparsify/config name-colliding tests)
- Full `cargo test -p tar-dedup --lib` → **211 passed** (202 baseline + 9 new);
  4 pre-existing failures (inventory ×2, scan ×2); 1 ignored. No new warnings.
- Notes: `assert_ne!` used for the symlink-absence check; `geteuid().is_root()`
  guard skips the chmod-0555 test under root; the strict tail assertion in
  `run_target_removal_failure_aborts_and_prevents_further_staging` is
  deterministic under the test config's `io_jobs: 1`.
- No `cargo fmt`, no `--release`.