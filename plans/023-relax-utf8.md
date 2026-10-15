# Plan: `relax_utf8` — strict-by-default UTF-8 path handling

Status: **agreed (2026-10-05), implementation round.**

## Goal

Entries with non-UTF-8 paths are currently transcribed with `to_string_lossy()`
(silent mangling). Make UTF-8 handling explicit:

- **Default (no flag): strict.** A non-UTF-8 path aborts the run with
  `Error::Config("Only UTF-8 Strings supported. Disable with --relax-utf8")`.
- **With `--relax-utf8` (process option, no CLI arg yet, `// TODO cli`):** the
  non-UTF-8 **entry is skipped** — `tracing::warn!` with the lossy name and
  `continue`; the lossy conversion is used for that report only.

User answers: warn+skip uses lossy conversion (1); strict = `Error::Config` for
now, expressive error variants are future work (2); the `--fail-fast` conflict
validation belongs in `try_from` (3) — but there is no CLI arg yet, so the
validation is deferred together with the flag (`// TODO cli`); display-only
`to_string_lossy()` stays (4).

## Config

### `config/process.rs` — shared `ProcessOptions`
```rust
/// Relax-UTF-8: skip entries whose path cannot be stored as UTF-8.
pub relax_utf8: bool, // TODO cli (--relax-utf8); --fail-fast conflict validated in try_from
```
Not `PathLayout`: it is a processing semantic shared by both pipelines and
forms a validation pair with `process.fail_fast` in `try_from`.

### `config/extract.rs` / `config/archive.rs`
`relax_utf8: false, // TODO cli` in every `ProcessOptions { … }` literal
(extract: try_from, for_place_test, for_scan_test, merge_over via
`merge_pick_copy`, DEFAULT_CONFIG; archive: try_from, merge_over, DEFAULT).
`try_from` gains a `// TODO cli` marker where the `relax_utf8 && fail_fast`
→ `Error::Config` check will live.

## The conversion primitive — `common/utf8.rs` (new)

```rust
pub trait PathText {
    /// Some(path-as-utf8) | None = skip (relax + invalid) | Err = abort (strict + invalid)
    fn to_utf8(&self, relax: bool) -> Result<Option<String>>;
}
impl PathText for Path      // via self.as_os_str().to_str()
impl PathText for PathBuf   // via self.as_os_str().to_str()
impl PathText for OsStr     // via self.to_str()

pub fn require_utf8(path: impl AsRef<Path>) -> Result<String>  // strict, no skip
```
Shared core `fn utf8_of_os(&OsStr) -> Option<String>`; `utf8_fail(relax)` emits
the `Error::Config` message or `Ok(None)`. `require_utf8` wraps the strict case
for the db store sites. `common.rs` gains `pub mod utf8;`.

## Call-site treatment (three buckets)

1. **Entry boundaries (skip-capable), via `to_utf8(relax)`:**
   - `archive/inventory.rs::handle_entry_base` — `Ok(None)` → warn + return
     Ok(()) (no stat, no row, no progress); covers both the walk
     (`handle_dir`) and `--files-from` lines.
   - `unarchive/scan.rs::entry_name(path, relax)` → `Result<Option<String>>`;
     call site: skip member (warn + `continue`) on `Ok(None)`. (Tar member
     names are content-ids today, so this is defensive.)
2. **DB stores (never skip; guaranteed post-boundary valid):** replace
   `to_string_lossy()` with `require_utf8(&p)?` —
   `db/inventory.rs:19,48,64`, `db/source.rs:35,43,70`,
   `db/place_prologue.rs:134,144`, `unarchive/rehash.rs:454,477`,
   `db/scan.rs:89`.
3. **Display/derivation only (unchanged `to_string_lossy()`):**
   `progress.rs:149`, `common/xattr.rs:58`, `config.rs` (archive stem),
   `config/compression.rs:157`, `common/files.rs:240`, `db/errors.rs:99`,
   `common/filter.rs:228,344`, `unarchive/place_prologue.rs:233,435`
   (valid-input processing).

## Tests

- `common/utf8.rs` `#[cfg(test)]`: valid → `Ok(Some)`, strict+invalid →
  `Err` containing `--relax-utf8`, relax+invalid → `Ok(None)`.
- Existing fixtures are ASCII; suite stays green (283 + new).

## Verification

- `cargo build -p tar-dedup -p tar-dedup-cli`
- `cargo test -p tar-dedup --lib`