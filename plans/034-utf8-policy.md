# Plan: UTF-8 policy — lossy working string + always-raw identity (2a)

Status: **agreed (2026-10-12), implementation round.**

## Policy matrix

| mode | non-UTF-8 path |
|------|----------------|
| strict (default) | `to_str()`; warn + **skip** entry |
| strict + `--fail-fast` | `to_str()`; **error + abort** |
| relax (with or without fail-fast) | `to_string_lossy()`; **never skip**; raw bytes always stored |

`--force` stays mutually exclusive with `--relax-utf8`.

Canonical working form = the (lossy) string, used by `files`/`source`/`out_tree`,
filters, transforms, strip/mapping. Raw bytes live in always-populated BLOB columns
and serve (a) archive-time fs access and (b) a faithful DB record for inspection.
Raw bytes are **never re-parsed as working paths** and **never used on extraction**
(extraction materializes from the string forms).

## 1. `common/utf8.rs` — policy API

```rust
pub struct SanitizedPath { pub text: String, pub raw: PathBuf }

/// Ok(Some)  -> proceed: .text = DB string (valid or lossy), .raw = original bytes
/// Ok(None)  -> strict + !fail-fast: warn + skip
/// Err       -> strict + fail-fast: abort (Error::Config)
pub fn sanitize_path(relax: bool, fail_fast: bool, path, path_type)
    -> Result<Option<SanitizedPath>>
```

`require_utf8` stays for assert sites. Rewrite module tests for the matrix.

## 2. Schema (identity on raw — 2a)

- `files`: `abs_path TEXT NOT NULL` (**remove UNIQUE**),
  `abs_path_raw BLOB NOT NULL UNIQUE`, `link_dst_raw BLOB` (doc-only).
- `source`: `abs_path_raw BLOB NOT NULL`, `original_path_raw BLOB NOT NULL`;
  `UNIQUE(source, abs_path)` -> `UNIQUE(source, abs_path_raw)`.

## 3. Row types (unix byte round-trip; non-unix stores abs_path bytes as raw)

- `NewFileRecord`: + `abs_path_raw: Option<PathBuf>` (None -> store abs_path
  bytes), + `link_dst_raw: Option<PathBuf>`.
- `StrippedRecord`: + `abs_path_raw: Option<PathBuf>` + `fn fs_path() -> &Path`
  (`abs_path_raw.as_deref().unwrap_or(&abs_path)`); update `SqlFileRow`
  `sql_columns`/`from_row`.
- `SourceRecord`: + `abs_path_raw`, + `original_path_raw`.
- `FileRecord`: unchanged (link_dst_raw never read).

## 4. `archive/inventory.rs` — apply policy at the 7 gates

`match sanitize_path(relax, fail_fast, …)?`: `None` skip, `Err` abort,
`Some(s)` proceed with `s.text` for DB strings and `s.raw` for raw columns; fs ops
(metadata, `read_link`, walk) keep the raw `&Path`. `handle_entry` ->
`NewFileRecord { abs_path: text, abs_path_raw: Some(raw), link_dst: text,
link_dst_raw: Some(raw_dst) }`.

## 5. Lookups (audit)

- `file_id_by_abs_path` / `abs_path_exists` -> match `abs_path_raw = :bytes(path)`.
- `db.rs:111` `insert_file_and_ref` preflight -> `record.abs_path_raw`.
- `inventory.rs:295/459` hold the raw `&Path` -> resolve via raw automatically.
- `db/source.rs` `add_get_source` -> gains raw params; INSERT OR IGNORE + SELECT
  keyed on `abs_path_raw`. `find_overlapping_source` stays on the working string
  (mapping, not identity).
- `db/inventory.rs` `insert_file` -> write `abs_path_raw` (+ `link_dst_raw`).

## 6. fs phases -> `fs_path()`

`hash.rs:202/243`, `sparsify.rs:253`, `dedup.rs` (pair open + canonical compare
`:424/426`; `DedupPair` from fs_path), `stage.rs` symlink targets. `tar_builder`
unaffected.

## 7. Extraction — unchanged

`place.rs` uses string forms; symlinks use `link_dst` (lossy under relax,
accepted). `link_dst_raw` is stored for the faithful record only.

## 8. Config / docs

- `config/process.rs` validate: `relax && force` -> Error only (drop `fail_fast`).
- `--relax-utf8` help (`cli.rs:402`, `:907`) + field doc: "lossily convert, never skip".

## 9. Deliberate exemptions (NOT touched)

- `db/scan.rs:30` cache payload names (`to_str` else `continue`).
- `unarchive/scan.rs:810-821` `entry_name` lossy-on-purpose reject path.
- `common/files.rs:66-72` `original_extension` (non-UTF-8 ext -> empty).
- `archive/inventory.rs:560-571` `os_str_from_bytes` (windows helper).
- `db/scan.rs:212` snapshot-path assert; `unarchive/rehash.rs:456/479` test helpers.
- Display-only `to_string_lossy` (`db/errors.rs`, `progress.rs`, `config.rs`,
  `config/compression.rs`, `common.rs`, `common/filter.rs:352`).
- `common/filter.rs:231` filter haystack — **superseded (2026-10-15)**: `test_match`
  now matches the raw path bytes (`fs_path()` / `regex::bytes::Regex`), so
  byte-oriented `--no-unicode` filters reach non-UTF-8 path bytes. Patterns remain
  UTF-8 text clues; the haystack is the original bytes.

## Edge cases (documented)

- `abs_path` non-unique: lossy rows may share a display string; identity is
  `abs_path_raw`. Audited: no remaining single-row-by-abs_path queries
  (`find_overlapping_source` is LIMIT 1 mapping).
- Non-UTF-8 dir: strict -> whole subtree skipped; relax -> whole subtree lossy+raw.
- Non-unix: raw mirrors abs_path (feature effectively unix).

## Verification

- `cargo build -p tar-dedup -p tar-dedup-cli`
- `cargo test -p tar-dedup --lib`
- New `utf8.rs` matrix tests + `abs_path_raw` write/read `fs_path` round-trip test.
