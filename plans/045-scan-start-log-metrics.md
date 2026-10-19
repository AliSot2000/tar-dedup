# Plan: real "starting scan" numbers + drop the corpus stub

Status: **implemented (2026-10-17).** Replaces the useless `corpus` log (0,0,0 or
total,total,total) with the real scan metrics, computed once in
`attach_db_progress`.

## Problem

`run_inner` logged `starting scan: total {}, processing {}, pending {}` from a
single `corpus = count_entries()` (or 0), so it always read `0,0,0` or
`N,N,N` — meaningless. The meaningful numbers are:
- total = `count_entries()`
- processing = `count_encoded_payloads()`
- pending = processing − `count_extracted_canonical()`

`attach_db_progress` already computes the first three of these, so the log should
reuse them, not re-query.

## Changes

- `attach_db_progress` now returns `(total, processing, pending)` and drops its
  commented-out TODO block.
- `run_inner` captures the metrics at the DB-attach point and logs
  `starting scan: total T, processing P, pending Q` (with the
  `(resume from member N)` suffix when resuming) once `scan` is in scope. A
  footer-less fresh pass (no catalog yet) logs
  `starting scan: no catalog yet (awaiting footer/manifest)` — no three-number
  line without the numbers.
- `install_database`'s `attach_db_progress(...)?` call discards the returned
  tuple (no behavior change).

## Verify

- `cargo build -p tar-dedup` and `--workspace` clean.
- `cargo test -p tar-dedup --lib` — 412 passed / 1 failed (pre-existing stale
  `clear_archive_meta_drops_only_byte_counters`); `unarchive::scan` 16/16.
