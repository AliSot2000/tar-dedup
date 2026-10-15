# Plan: opt-in `.to_panic()` across `db/*` functions

## Problem
The work-directory SQLite DB lives on a trusted filesystem. A DB that is missing,
truncated, slow, full, or returning garbage *values* is a programming bug to debug — not
a runtime scenario to handle. We want the `err-to-panic` debug feature to surface such a
failure as a panic with an origin backtrace at the exact DB boundary, while leaving a
default (feature-off) build byte-identical behavior-wise.

## Rule — panic where the error is *produced* from the trusted DB

### Rule A — add `.to_panic()`
1. **Direct rusqlite `?`** → `...to_panic()?` for `execute`, `execute_batch`,
   `prepare`, `prepare_cached`, `transaction`, `commit`, `query_row`, `query_map`,
   `query`, `rows.next()`, `OptionalExtension::optional()`, plus rusqlite `?` inside
   row-map closures (`row.get(...)`, `query_map` mappers, `parse_row`,
   `optional_sha1`, ...). Also `schema.rs::initialize` (schema init is `expect`-class).
2. **rusqlite tail conversions without `?`** → wrap before `.map_err(Into::into)`:
   `rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)`,
   `....optional().to_panic().map_err(Into::into)`.
3. **DB-value corruption at the observation point** (user-confirmed: a bad *value*
   read back from a healthy DB is the same trust violation):
   - `meta.rs::get_typed` → `MetaEntry::decode(key, &raw).to_panic()?` and the
     "internal meta type mismatch" tail `.to_panic()` (single observation point covers
     all `decode` parse failures; `decode` stays a pure parser — sole caller is
     `get_typed`).
   - `common.rs` `parse_phase` / `parse_ftype` / `optional_rfc3339` → `.to_panic()`
     before their `map_err(FromSqlConversionFailure)`.
   - `inventory.rs` meta reads (`archive_max_workers`, `snapshot_taken_at` Config) →
     `.to_panic()`.
   - `errors.rs` `ErrorPhase::parse` is converted to a rusqlite error at `parse_row`,
     so it is automatically covered by Rule A1/A2; no separate edit.
   - `errors.rs` recorder flush (`insert_errors` txn/prepare/execute/commit) **also
     panics** — losing a recorder flush is itself fatal (user-confirmed).

### Rule B — leave as-is (no edits)
- Cross-`db/*`-function calls (`load_extract_scan_state(conn)?`,
  `meta::set_scan_*` inside `save_extract_scan_state`, `flags::set_file_flag(...)?`,
  `with_meta_txn`, ...). Callees panic at their own leaf first, so the outer `?` never
  sees an Err under the feature; a plain `?` is therefore noise. E.g.
  `record_snapshot_ingested` gets **zero changes**.
- Real filesystem errors (`scan.rs` `fs::read_dir` / `remove_file` / `copy` →
  `Error::io`). Those are not DB-trust; they keep propagating up to normal pipeline
  error handling.
- `content_id.rs` entirely (user-confirmed skip): `parse_content_id` is used
  non-fatally on FS-derived cache filenames (`let Ok(...) else continue`).

## Surface
~250 try-op `?` sites across 19 modules + ~20 rusqlite tail conversions. Heaviest:
`common.rs`, `scan.rs`, `meta.rs`, `inventory.rs`, `tar_writer.rs`, `permissions.rs`.
`db.rs` (the `Database` adapter impl) is explicitly out of scope.

Each edited module gains `use crate::error::ToPanic;`.

## Steps
1. Apply Rule A per module, wrapping in place (`foo.to_panic()?`,
   `...to_panic().map_err(...)`); no reformatting, no new helpers.
2. Verify `cargo check` (default = pass-through must stay behavior-identical),
   `cargo check --features err-to-panic`, `cargo check -p tar-dedup-cli`.
3. Smoke: default archive+extract run unchanged; under `--features
   tar-dedup/err-to-panic`, a corrupted work-DB (bad meta value / missing table) panics
   with origin backtrace instead of returning `Error::Database(...)`.

## Concerns
- `errors.rs::insert_errors` is the recorder flush path for non-fatal per-file errors;
  a DB failure there panics under the feature. Accepted: an SQLite failure during
  error-recording is fatal, not swallowable.