# Plan: scan.rs progress accounting

Status: **agreed (2026-10-17), tidy-up — no tests.** Fixes the scan-phase progress
bars in `unarchive/scan.rs` (global + phase + per-member increment). Pure
accounting refactor; no behavior change to extraction.

## Problem

- `run_inner` (scan.rs:156-162) calls `set_phase_total(count_entries())` on the
  scan phase bar — which is `BarKind::Counter` (total invisible) — and only at the
  initial load (`// TODO this seems odd`). Mid-scan DB installs never set any
  progress; nothing accounts for non-encoded rows or prior-pass progress.
- The loop `inc_both(1)` per tar member (scan.rs:268) counts catalog members
  (manifest/snapshot) too, so the phase bar overshoots the payload total.

## Changes

1. **DB method** `count_encoded_payloads()` in `db/scan.rs` + `db.rs` facade:
   `SELECT COUNT(*) FROM files WHERE canonical_id = id AND (flags & :appended) != 0`
   (matches the `count_missing_payloads` flag style). "Encoded" = archive payload
   members.

2. **Helper** `attach_db_progress(db: &Database, progress: &ProgressBarSet)`
   in `unarchive/scan.rs`, called right after every `rt.db = Some(...)`:
   - `progress.set_table_size(db.count_entries()?)` — global total (later
     adjusted by place_prologue).
   - `progress.set_phase_total(encoded)` + `progress.set_phase_kind(BarKind::Count)`
     — spinner → fraction bar once a total is known.
   - `progress.inc_global(total - encoded)` — eager promotion of non-encoded rows.
   - `progress.inc_both(db.count_extracted_canonical()?)` — already-extracted seed
     (option 1: computed at load). `0` on fresh installs (normalize clears
     `FileExtracted`); nonzero on resume (no normalize).

3. **Call sites**:
   - `run_inner` initial load (resume / footer): replace the
     `set_phase_total(count_entries())` block; keep the recorder flush-limit line.
   - `install_database`: after `rt.db = Some(opened)` — covers the leading-manifest
     install and the force snapshot installs.

4. **Force mid-scan attach**: in `process_entry` `(SNAPSHOT_TAR_NAME, true)` with
   `force && db.is_none()`, after the buffered fids are flagged `FileExtracted`
   (scan.rs:471-478), `rt.progress.inc_both(ref_db.count_extracted_canonical()?)`
   — accounts payloads extracted during the pre-DB window.

5. **Loop increment**: remove the per-member `rt.progress.inc_both(1)` (scan.rs:268);
   add `if rt.db.is_some() { rt.progress.inc_both(1); }` at the **top** of the
   payload arm in `process_entry` (before the eager-filter gate), so every payload
   member counts — including filter-skipped and force-failed ones — matching
   `phase_total = encoded`.

No `enter_phase` change needed (scan stays `Counter`; `set_phase_kind` flips it).

## Verify

- `nix develop -c cargo build -p tar-dedup`
- `nix develop -c cargo test -p tar-dedup --lib unarchive::scan` (behavior unchanged)
