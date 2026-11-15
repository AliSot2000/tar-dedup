# 094 — Placement prologue: cooperative exit + re-entrant recovery

Status: **implemented (2026-11-15).**

## Background

The extract interrupt handler only forces a WAL checkpoint for `scan`/`rehash`.
The old comment grouped `prologue`/`place`/`permissions` as "must restart
cleanly", which was ambiguous: those phases commit per-batch row state and are
re-entrant; the missing piece was that `place_prologue` never checked
`Shutdown`, so a graceful stop only took effect at the next phase boundary.

## Changes

### `place_prologue` is now cooperatively interruptible

`check_between_files` is checked:

- before each prologue stage and before setting `placement_prologue_done`;
- per row in `apply_renames`;
- per batch in `populate_out_tree_abs` / `populate_out_tree_rel`;
- per batch in `ensure_parent` (now takes `ExtractRTArgs`);
- before the bulk canonical updates and per source for
  `HardLinkGrouping::Source`.

A graceful interrupt now returns `Interrupted` with the phase still
`PlacementPrologue`; the next run re-enters the prologue.

### Partial prologue state is safe to re-run

The prologue is not a single transaction; out_tree inserts commit per batch and
`new_name` updates are per-row. To make re-entry safe:

- `insert_ref_out_rows` is idempotent (`INSERT … WHERE NOT EXISTS` on
  `(out_id, source_id, file_id)`), so a partially/fully populated mapping is not
  duplicated.
- `insert_out_tree_rows` only records a `MaterializeConflict` when that
  `(out_id, file_id)` mapping is not already present in `ref_out`; re-entry no
  longer duplicates prologue errors.
- `out_tree_built` is only set after population + `ensure_parent`; if
  interrupted earlier, the next run re-populates and reuses resident rows
  through the UNIQUE path.
- `new_name` overwrites and hardlink-canonical updates are idempotent;
  `strip_common_prefix` runs in one transaction and is a no-op on re-run.

### Restart semantics

- `place` and `permissions` remain per-row resumable: they skip rows already
  marked through the DB flags and do not need a mid-phase checkpoint.
- A partial `--bypass-archive` scan is intentionally not resumable mid-walk: the
  next run rescans the product dir from the beginning (documented on
  `run_bypass`).

## Tests

- `run_stops_gracefully_and_resumes`: graceful shutdown returns `Interrupted`
  before doing work; a detached rerun completes and records the mappings.
- `run_reuses_partial_out_tree_without_duplicate_ref_out`: pre-seeded partial
  `out_tree` + `ref_out` rows are reused; the re-run leaves exactly one
  `ref_out` row per mapping.
- `reinserting_a_conflict_does_not_duplicate_the_error`: the same clobber
  re-inserted does not add a second `MaterializeConflict` row.
- `cargo test -p tar-dedup-lib --lib`: 779 passed.

## Files

`unarchive/place_prologue.rs`, `db/place_prologue.rs`, `db/place.rs`,
`unarchive/scan.rs` (doc), `unarchive.rs` (checkpoint comment).
