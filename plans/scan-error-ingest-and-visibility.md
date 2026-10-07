# Scan no-DB error ingest + phase-function visibility audit

Agreed design for two changes: (1) finalize the no-DB error path in
`unarchive/scan.rs` so buffered errors are ingested the moment a database
materializes, and (2) tighten the visibility of phase functions per the
module-scope rules below.

## 1. scan.rs: ingest no-DB errors as soon as a DB appears

### Current state (already in the worktree)
- `force_buffer: Option<(Vec<FileId>, Vec<FileId>)>` — `Some` iff `--force`;
  successes in `.0`, failures in `.1`.
- Failure path (`process_entry` B2 arm): DB present →
  `set_file_flag(fid, ErrorWhileExtracting)`; no DB → `fid` pushed to `.1`.
  The failing `FileStatError` (with the cache destination path) is also
  buffered in the speculative [`Recorder`] via `record_file_scan_error`.
- Snapshot install (`SNAPSHOT_TAR_NAME` arm, force + no DB): drains `.0` →
  `FileExtracted`, `.1` → `ErrorWhileExtracting`, then
  `apply_snapshot_promote_unarchived`.

### Gap
The buffered `FileStatError` rows are only persisted to the `errors` table
indirectly — today they survive via the end-of-scan gate's force-arm flush
(`flush_scan_recorder(recorder, Some(&sdb))`) or a later abort/`Drop`. If
neither runs, the drafts stay in the speculative recorder and are lost at
`Drop` (no DB attached → draft loss, `db.rs`).

### Changes
1. In the snapshot-install arm, after the buffer drain + flag set + promote,
   add `flush_scan_recorder(recorder, Some(&ref_db))?` so the buffered scan
   `FileStatError`s are ingested the moment the DB materializes.
2. Keep the failure buffer as `Vec<FileId>` (no name resolution; the `fid`
   from the member content id uniquely identifies the row, and the draft
   already carries the path).
3. Remove the now-stale TODO comment (lines 36-37, "Create separate array to
   push files that failed into…") — the feature is implemented.
4. Testing: `scan_no_db_failure_buffered_then_flagged_on_snapshot` already
   asserts `count_errors >= 1`; with the earlier flush it still passes.
   Success-buffer test unaffected. Full suite re-run (expect 318).

### Accepted limitation (unchanged)
If no DB ever materializes (head-truncated, no snapshot), the run aborts with
"Archive did not contain database" and buffered fids/errors are dropped —
unavoidable without a DB target.

## 2. Phase-function visibility audit

Criterion: module-scope. "Used in multiple locations" = spanning modules
within one phase (→ `pub`); one module only (→ `private`); spanning both
pipelines (→ already in `common/`).

### Make private (used only inside their own phase module; tests use
`use super::*`, so unaffected)

`archive/inventory.rs` (keep `run` pub):
- `handle_dir`, `handle_entry_base`, `handle_entry` (×2), `inner_file_type`,
  `no_treat_path`

`unarchive/place.rs` (keep `run` pub):
- `prepare_extraction_dir`, `copy_canonicals_to_source`,
  `materialize_link_tree`, `materialize_files`, `materialize_hardlinks`,
  `materialize_others`, `status_message_rebuilding`, `build_path`

`unarchive/permissions.rs` (keep `run` pub):
- `apply_permissions_link_sources`

`unarchive/place_prologue.rs`:
- `populate_out_tree` → private, and drop the dead re-export
  `pub use place_prologue::populate_out_tree;` in `unarchive.rs:10`
  (nothing calls it).

### Keep public (cross-module within one phase, or phase entry called by the
orchestrator)
- All phase `run` entries (`archive::*::run`, `unarchive::{scan,rehash,place,
  place_prologue,permissions}::run`).
- `archive::filter::ingest_filters`.
- `unarchive::filter::{run, ingest_filters}` — `run` has two call sites
  (orchestrator Filter phase + `scan.rs::store_filter_buffer`).
- `ParseFilterBuffer::{add_include, add_exclude, count_includes, write_to_db}`.

### Move to common: none
All cross-pipeline sharing already lives in `common/` (`common/filter.rs`,
`common/files.rs`, `common/perms.rs`, `common/start.rs`, `common/cleanup.rs`).
Verified: `archive/` imports no `crate::unarchive::`; `unarchive/` only pulls
`archive_footer`, `tar_reader`, `common`, `db`, `config`, etc.

## Verification
- `cargo test -p tar-dedup --lib` (dev profile; expect 318)
- `cargo build -p tar-dedup-cli`
- No `cargo fmt`; preserve surrounding style.