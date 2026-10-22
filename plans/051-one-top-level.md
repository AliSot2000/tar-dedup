# Plan: apply `--one-top-level` to the effective extraction root

Status: **agreed (2026-10-22).** `--one-top-level` currently only affects the
`--clean-target` block in `place.rs` (it removes/recreates `<ed>/<otl>`), but the
out_tree rows are still built at `<ed>/…`, so extracted content never lands under the
subdirectory. This plan makes the whole extract pipeline target `<ed>/<otl>/…` when set.

## CLI: bare `--one-top-level` defaults to the archive stem

`--one-top-level` is `Option<PathBuf>` and clap rejects a bare flag ("a value is
required"). GNU tar defaults the bare flag to the archive name; a bare flag that
resolves to "no subdirectory" would be a silent no-op, so we mirror GNU tar:

- `cli.rs`: `one_top_level: Option<Option<PathBuf>>` with `num_args = 0..=1` →
  no flag → `None`, bare flag → `Some(None)`, `--one-top-level DIR` →
  `Some(Some(DIR))`. (`default_missing_value = ""` is silently ignored by clap 4.6 for
  `Option<PathBuf>` — verified in isolation; the nested-Option idiom needs no sentinel.)
- `config/extract.rs` builder: `None` → `None`, `Some(None)` → `Some(archive_stem(...))`
  (existing `archive_stem`, strips compression/`.tar` suffixes), `Some(Some(otl))` →
  `Some(otl.clean())`.

## Core: effective extraction root

Add to `ExtractConfig` (`config/extract.rs`):

```rust
/// Effective extraction root: `<ed>` joined with `--one-top-level` when set.
pub fn effective_extraction_root(&self) -> PathBuf {
    match &self.placement.one_top_level {
        Some(otl) => self.paths.extraction_root().join(otl),
        None => self.paths.extraction_root().to_path_buf(),
    }
}
```

The out_tree abs_paths are minted in `place_prologue.rs` by joining `root` in
`build_new_out_tree_rows`; baking the prefix in there makes every downstream phase
(dir tree, materialize, link_tree, permissions) follow automatically because they read
`out_tree.abs_path`.

### `place_prologue.rs`
- `populate_out_tree_abs`: `let root = rt.config.effective_extraction_root();` (was
  `&rt.config.paths.extraction_root()`).
- `populate_out_tree_rel`: same swap.
- `build_new_out_tree_rows` / `ensure_parent` unchanged (they take the root / read rows).

### `place.rs` clean block (lines 64-73)
Keep the `one_top_level.is_some()` guard (so `--clean-target` without otl stays a
no-op), drop the manual `expect`+join and use `effective_extraction_root()`.

### `unarchive.rs` log (line 183)
Log `effective_extraction_root()`.

## Verified: downstream needs no change

- **link_tree**: `relative_pardirs_to_dir(extraction_root(), out.abs_path)`
  (place.rs:865) yields `depth` `../`-s from `<ed>`; an otl path `<ed>/<otl>/f` gives
  `../` so the symlink target resolves to `<ed>/.sources/…` — correct. `.sources`
  stays at `<ed>` (helper dir, not extracted content).
- **`build_path`**: `already_checked` seeds at `<ed>` (place.rs:145) and creates
  `<ed>/<otl>` on the walk; the `prefix.len() >= extraction_root().len()` assert holds.
- **`validate_name`** already restricts otl to a single clean name → the join is safe.

## Tests (`place_prologue.rs`, via `PrologueWorld` tune)

- `one_top_level = Some("otl")`, abs mode → out_tree rows under `<ed>/otl/…`.
- Same, rel mode → `<ed>/otl/<rel-target>`.
- `run()` smoke with otl + transform → out_tree under `<ed>/otl/…`.
- Existing tests unchanged (otl defaults `None`).

## Files touched

- `crates/tar-dedup/src/cli.rs`
- `crates/tar-dedup/src/config/extract.rs`
- `crates/tar-dedup/src/unarchive/place_prologue.rs`
- `crates/tar-dedup/src/unarchive/place.rs`
- `crates/tar-dedup/src/unarchive.rs`
- `plans/051-one-top-level.md`

## Verification

- `cargo build -p tar-dedup-cli`
- `cargo test -p tar-dedup --lib -- unarchive::place_prologue`
- full `cargo test -p tar-dedup --lib` (expect only the pre-existing
  `clear_archive_meta_drops_only_byte_counters` failure)
