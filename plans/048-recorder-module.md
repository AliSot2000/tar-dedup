# Plan: move `Recorder` into `db/recorder.rs`

Status: **agreed (2026-10-20).** `db.rs` has grown to ~1,560 lines; the `Recorder`
(estruct + impl + Drop, ~230 lines) is a self-contained chunk that belongs in its own
module, mirroring the other `db/*` submodules.

## Scope

Move only `Recorder` (db.rs:1275–1505: module doc comment, `pub struct Recorder<'a>`,
`impl<'a> Recorder<'a>`, `impl Drop for Recorder<'_>`) into a new `db/recorder.rs`.
`RecordDraft`, `ErrorPhase`, `ErrorRecord` already live in `db/errors.rs`.

## Verified: no changes outside `db/` + `db.rs`

Every consumer imports `Recorder` as `crate::db::Recorder` (archive/dedup|filter|hash|
inventory|sparsify|stage|tar_builder, common/filter, unarchive/filter|permissions|place|
rehash|scan|unarchive, db/inventory). Keeping a `pub use recorder::Recorder;` re-export in
`db.rs` preserves that path, so no external file, `lib.rs`, `Cargo.toml`, or rustdoc link
changes. `Database::insert_errors` is already `pub`; `recorder` is a child module of `db`,
so it reaches the private `mod errors` via the existing `pub use errors::{…}` re-export.
`db.rs`'s own `#[cfg(test)] mod tests` does not reference `Recorder`.

## Changes

### New `crates/tar-dedup/src/db/recorder.rs`
- Move the `Recorder` block verbatim.
- Add imports (currently implicit inside `db.rs`):
  - `use crate::common::defaults::DEFAULT_AUTO_FLUSH_LIMIT;`
  - `use crate::db::flags;` (for `flags::ErrorFlags` / `flags::ErrorFlag`)
  - `use crate::db::types::{FileId, OutTreeId};`
  - `use crate::db::{Database, ErrorPhase, RecordDraft};`
  - `use crate::error::{Error, FileStatError, Result};`

### `db.rs`
- Add `mod recorder;` between `mod rehash;` and `mod scan;`.
- Add `pub use recorder::Recorder;` with the other `pub use`s (~line 44–50).
- Remove the `Recorder` block (1275–1505).
- Remove the now-unused `use crate::common::defaults::DEFAULT_AUTO_FLUSH_LIMIT;` (line 51;
  grep confirms it is referenced only by the `Recorder` code).

## Verification
- `cargo build -p tar-dedup`
- `cargo test -p tar-dedup --lib` (expect 448 passed + the pre-existing
  `clear_archive_meta_drops_only_byte_counters` blocker).
