# 083 — `.tdd` canonical suffix + `--strip-common-prefix`

Status: **approved — implementing.**

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
- **LCP in Rust**: scalar component-wise pass over one `SELECT abs_path` (relative part);
  early return when only `/` is shared. (Component-wise LCP is scalar logic SQLite has no
  clean primitive for; the heavy N-row rewrite stays in one SQL statement, so the two don't
  duplicate each other.)
- **Single `UPDATE`** (one transaction):
  `UPDATE out_tree SET abs_path = :root || substr(abs_path, length(:root) + :lcp_len)`
  for rows under the common prefix.
- **`DELETE FROM out_tree WHERE abs_path = :root`**.
- **Re-run `ensure_parent`** (component-aligned; `canonical_id`/flags/refs untouched —
  elected later by `prepare_hardlink_canonicals`).

### Tests
- `place_prologue` unit: deep abs `-i` → rows re-rooted, top row deleted (absent),
  `ensure_parent` holds, no-op with divergent roots.
- Config test: flag → `PlacementOptions.strip_common_prefix`.
- Integration `strip_common_prefix.sh`: archive nested absolute source, extract with the
  flag, assert files land directly under the extract root.

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