# Plan: drop scan_complete + simplify store_progress_in_db

Status: **implemented (2026-10-17).** Removes the unused `ExtractScanState::scan_complete`
field (and its `scan_tar_complete` meta key) and replaces the load+merge in
`store_progress_in_db` with a direct write of the cumulative `scan` state.

## Rationale

- `scan_complete` was never read for control flow — only persisted and asserted in
  tests; the end-of-scan gate relies on `tally_scan_remainders`, not the flag.
  Dropping it removes the per-invocation reset (`scan.scan_complete = false`) that
  clobbered the DB-loaded value, per "trust the DB" semantics.
- `store_progress_in_db` merged the freshly loaded persisted state with `scan`
  (`|=`/`max`). On resume `scan` is itself loaded from the DB at the start of
  `run_inner`, and on a first pass (or mid-scan force install) the DB holds no
  extract-scan meta, so the merge is a no-op in every reachable case — writing
  `scan` directly is equivalent.

## Changes

- `db/extract.rs`: remove `scan_complete` field.
- `db/scan.rs`: drop `set_scan_tar_complete`/`get_scan_tar_complete` calls from
  `save_extract_scan_state` / `load_extract_scan_state`.
- `db/meta.rs`: remove `MetaKey::ScanTarComplete` variant, its `as_str`/`parse`/
  `all()` entries, `MetaEntry::ScanTarComplete`, its `key`/`encode`/`decode` arms,
  and the `get_scan_tar_complete`/`set_scan_tar_complete` functions.
- `unarchive/scan.rs`: remove the `if !scan.nt_stopped { scan.scan_complete = true; }`
  block; `store_progress_in_db` now writes `scan` directly + `checkpoint()`; drop
  the three `state.scan_complete` test asserts.

## Verify

- `cargo build -p tar-dedup` and `--workspace` clean.
- `cargo test -p tar-dedup --lib` — 412 passed / 1 failed (pre-existing stale
  `clear_archive_meta_drops_only_byte_counters`); `unarchive::scan` 16/16.
