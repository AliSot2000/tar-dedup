# 083 — `.tdd` canonical suffix + `--strip-common-prefix`

Status: part 2 (`--strip-common-prefix`) **implemented**; part 1 (`.tdd`) **implemented**.

Part-2 notes (SQL-first, per review):
- LCP computed from a single SQL aggregate `SELECT MIN(abs_path), MAX(abs_path)
  FROM out_tree WHERE id > 0` (the component-wise LCP of a set equals that of its
  lexicographic min/max), then compared in Rust over only those two rows.
- Re-root via one `UPDATE out_tree SET abs_path = :root || substr(abs_path,
  length(:lcp)+1) WHERE …` guarded to rows under the prefix; one `DELETE … =
  :root`. Both in one transaction (`db/place_prologue.rs::strip_common_prefix`).
- Wired in `unarchive/place_prologue::run` after `populate_out_tree`, before
  canonical election; re-runs `ensure_parent` afterwards.
- `id > 0` excludes the reserved `link_dst` sentinel row (abs_path `''`), which
  would otherwise become the MIN and nullify the prefix.

## Scope

1. Make `stem.tdd.tar(.compression)` the canonical (in fact the only) recognized tar-dedup
   archive suffix, per the 3-branch detection rule.
2. Add an extract `--strip-common-prefix` flag that re-roots the `out_tree` under
   `effective_extraction_root()` by deleting the collapsed top (source) directory.

## 1. `.tdd` suffix handling

### Recognized suffix set (mirrors the current resolution, prefixed with `.tdd`)

| Suffix | Format |
|--------|--------|
| `stem.tdd.tar.xz`, `stem.tdd.txz` | `Xz` |
| `stem.tdd.tar.gz`, `stem.tdd.tgz` | `Gz` |
| `stem.tdd.tar.bz2`, `stem.tdd.tbz2`, `stem.tdd.tbz` | `Bz2` |
| `stem.tdd.tar.zst`, `stem.tdd.tzst` | `Zstd` |
| `stem.tdd.tar` | `None` |

Legacy plain `.tar*` / `.astar` forms are **not** recognized.

### `resolve_compression(flags, path)` — 3-branch rule

1. **Auto-detect** the name against the suffix set. If it matches → use that format
   (when no compression flag).
2. **A compression flag is given but auto-detect failed** → use the flag's format and
   log `WARN: archive extension does not indicate <format>; using --<flag>` before
   extraction begins. *Permissive override, not an error.* Covers
   `-f backup.tdd.tar -J` (warn + xz) and `-f backup.tar.xz -J` (warn + xz).
3. **No flag and no auto-detect** → assume `stem.tdd.tar` (`None`); emit
   `INFO: Failed to infer archive type. Attempting regular .tdd.tar archive`; proceed.

`infer_compression_from_suffix` reduces to the `.tdd`-only matcher.

### `archive_stem`
Strip the full `.tdd` set → stem (`backup.tdd.tar.xz` → `backup`), so `{stem}.astage` /
`{stem}.estage` and `--one-top-level` default stay `backup`.

### Help / docs
`cli.rs` compression/format hint text and `Readme.md` examples → `.tdd.tar.xz`.

### Integration + fixtures
`integration/run.sh` `ar`/`ex`/`bin` default archive `$RUN/out.astar` → `$RUN/out.tdd.tar`;
update scenario scripts referencing `out.astar` / `reset.astar`; config-test fixtures
(`backup.tar`, `x.tar`, `out.tar`) → `.tdd.tar*` where a compression inference is asserted.
The derived work-dir name `out.astage` stays the same (stem is still `out`).

## 2. `--strip-common-prefix` (extract `out_tree` post-pass)

### Semantics
Strip the **longest common path prefix** across all `out_tree.abs_path` values *relative to
`effective_extraction_root`* (component-wise), re-root the remainders at
`effective_extraction_root`, **delete** the row that collapses to `effective_extraction_root`
(the source's top directory — so archived metadata is never applied to the root), and
**re-run `ensure_parent`** (fast; recreates missing parents as `file_id: NULL` ancestor rows
the permission pass already skips). No-op when only `/` is shared; never strips above the root.

### CLI + config
`ExtractArgs --strip-common-prefix` (Placement heading) →
`PlacementOptions.strip_common_prefix: bool` (default `false`) + Default + `Validate`.

### SQL-first implementation (`db/place_prologue.rs` + `unarchive/place_prologue.rs`)
- **LCP by SQL aggregate**: `out_tree_common_prefix` runs `SELECT MIN(abs_path),
  MAX(abs_path) FROM out_tree WHERE id > 0`; the component-wise LCP of a set equals that
  of its lexicographic min/max, so Rust compares only those two rows (no full-scan into
  RAM). No-op when only `/` is shared; never strips above the root.
  Verified by `common_prefix_min_max_agrees_with_full_scan` (brute-force cross-check).
- **Single `UPDATE`** (one transaction):
  `UPDATE out_tree SET abs_path = :root || substr(abs_path, length(:root) + :lcp_len)`
  for rows under the common prefix, then **`DELETE FROM out_tree WHERE abs_path = :root`**
  (the row that collapses to the extraction root — the source's top directory — so archived
  metadata is never applied to the root). Both wrapped in
  `strip_common_prefix(conn, root, shared)`.
- **`DELETE` removes exactly 0-or-1 rows**: `out_tree.abs_path TEXT NOT NULL UNIQUE` makes
  duplicate collapsed rows impossible, and the strip is injective (distinct remainders)→
  no two rows can collide at the root, so no `UNIQUE` violation can arise.
- **Re-run `ensure_parent`**, then `delete_out_tree_root` drops the re-notioned root row so
  metadata/owners never apply to `effective_extraction_root()`
  (`canonical_id`/flags/refs untouched — elected later by `prepare_hardlink_canonicals`).
- **Real rows are clean paths** (no trailing slash, via `root.join(*).clean()`), so a
  collapsed dir maps exactly to `:root` and matches the DELETE; a hypothetical trailing
  slash would yield `:root || '/'` and survive (tested via clean-path fixtures only).

### Tests (all green)
- `db::place_prologue`: shared-subdir re-root + top drop; divergent/empty no-op;
  single-component prefix; min/max aggregate agrees with full-scan brute force
  (covers sibling rows that shorten or nullify the prefix); nested-tree
  `a/,a/b/,a/b/c/` collapses only the exact LCP row; dir-sibling `/ed/mnt` shrinks the
  LCP to `mnt` and deletes only the `/ed/mnt` row; no-dir-sibling collapses `/ed/mnt/drive`
  → root; `delete_out_tree_root` drops the re-notioned root.
- `unarchive::place_prologue`: `run` re-roots + drops top; divergent roots no-op;
  works in `link_tree` mode.
- Config test: flag → `PlacementOptions.strip_common_prefix`.
- Integration `strip_common_prefix.sh` (part 1 only): archive nested absolute source,
  extract with the flag, assert files land directly under the extract root.

## 3. Verification
- `cargo test -p tar-dedup-lib --lib` (+ `--features test-compression`).
- `cargo build --release`.
- Local `integration/run.sh`.
- VM: sync, `cargo test --features test-all` (`TMPDIR=$HOME/tmp-it`), incl. `integration_shell`.

## Files touched
`config/compression.rs`, `config.rs`, `config/extract.rs`, `cli.rs`,
`unarchive/place_prologue.rs`, `db/place_prologue.rs`, `integration/run.sh`,
`integration/scenarios/*`, `Readme.md`, plus the relevant `#[cfg(test)]` blocks.

## Notes / prior-work refs
- `082` (compression JSON + `--bypass-archive`) is landable; `.tdd` is a naming-layer change
  on top (the `.tdd` marker is orthogonal to the footer/format, which stays `.tar`-shaped).
- `--strip-common-prefix` runs inside `place_prologue` after `populate_out_tree`, before
  `prepare_hardlink_canonicals` (canonical election must see the final `abs_path` set).