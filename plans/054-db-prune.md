# Plan: prune db/ — remove unused, gate + rename test-exclusive

Status: **agreed (2026-10-23).** Scope is the `db` module only: `src/db.rs`
facade + `src/db/*.rs`. References counted workspace-wide (lib, cli, sparse-cp,
examples), comments/strings stripped, `#[test]` fns excluded (the harness
invokes them by attribute). Rationale (user): binary project, not a framework —
dead pub API costs nothing to re-add, so remove rather than keep.

## Step 1 — remove the 7 truly-unused `db/` functions

- `db.rs:587` `pub fn mark_archive_session_aborted` — facade; the underlying
  `tar_writer::mark_session_aborted` is reached via `abort_incomplete_archive_session`.
- `db/common.rs:530` `pub(crate) fn delete_meta` — already compiler-flagged.
- `db/recorder.rs:73` `pub fn detach`
- `db/recorder.rs:78` `pub fn set_auto_flush` — only ref is its own doc comment.
- `db/types.rs:315` `pub fn is_extract_phase` (`ExtractPipelinePhase`)
- `db/types.rs:341` `pub fn is_exclusion_filter` (`FilterExpression`)
- `db/types.rs:344` `pub fn is_inclusion_filter` (`FilterExpression`)

## Step 2 — `files_in_phase`: `t_` prefix + `#[cfg(test)]` gate

`db.rs:188 pub fn files_in_phase<R: SqlFileRow>` is the only `db/` function used
exclusively by the test harness (`test_support.rs:389`, `archive/inventory.rs:813` —
both in `#[cfg(test)]` code). Rename to `Database::t_files_in_phase`, gate with
`#[cfg(test)]` (correct gate: every caller is test-gated; `debug_assertions`
would leave it dangling in a plain debug `cargo build`), update the 2 call
sites. Consistent with existing conventions (`test_support.rs` is a `#[cfg(test)]`
module; `for_place_test`/`for_scan_test` are `#[cfg(test)]` methods).

No non-`#[test]` `db/` function name contains a `test` marker, so the
"drop the literal `test`" rule has nothing to apply here.

## Files touched

- `crates/tar-dedup/src/db.rs`
- `crates/tar-dedup/src/db/common.rs`
- `crates/tar-dedup/src/db/recorder.rs`
- `crates/tar-dedup/src/db/types.rs`
- `crates/tar-dedup/src/test_support.rs` (call site)
- `crates/tar-dedup/src/archive/inventory.rs` (call site)
- `plans/054-db-prune.md`

## Verification

- `cargo build -p tar-dedup` — `delete_meta` "never used" warning gone.
- `cargo test -p tar-dedup --lib` (default; pre-existing blocker only).
- `cargo test -p tar-dedup --lib --features test-all` (gated).
- `rg` sweep: removed names gone; only `t_files_in_phase` remains.

## Implementation notes (2026-10-23)

Done. Removal of the 7 functions plus the rename/gate as planned. Follow-ups found
during the build:

- `db/common.rs list_files_entries_in_phase` (the raw backing of `files_in_phase`)
  became test-exclusive too once its only caller was gated — the non-test build
  flagged it "never used" (private-module `pub fn`). Gated it `#[cfg(test)]` as
  well.
- The dangling doc link `[set_auto_flush]` on `Recorder::auto_flush` was dropped.

Verification: `cargo build -p tar-dedup` clean (only pre-existing `error_guarded`
macro + `place.rs:402` `;;` warnings remain); default suite 554 passed / 1 failed
(pre-existing `clear_archive_meta_drops_only_byte_counters` blocker); `test-all`
563 passed / 1 failed (same blocker). No `cargo doc` link references the removed
names.

