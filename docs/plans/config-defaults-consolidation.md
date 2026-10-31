# Consolidate config defaults (single source + umbrella groups)

Draft for discussion — describes the problem and a proposed design. Not yet
implemented; revisit before acting.

## Problem: defaults are defined in three places

1. `common/defaults.rs` — low-level tunables (`IO_BUF_SIZE`, `BATCH_SIZE`,
   `WORK/OUT/FEED/DRAIN_CAPACITY`, `DEFAULT_AUTO_FLUSH_LIMIT`, tar member
   names). Already the single source for the 6 tuning consts (cli + config
   `ProcessOptions::default()` reference them).
2. `cli.rs` — ~60× `default_value_t = false` on `SetTrue` bools (redundant:
   clap already yields `false` for an absent flag), plus literal/enum defaults:
   `page_size = 4096`, `min_pages = 0`, `strip_components = 0`,
   `DedupMode::Regular`, `ArchiveOrdering::ExtSizeId`,
   `ConflictPolicy::Replace`, `HardLinkGrouping::Global`,
   `MapResolutionTarget::NameId`, `ProgressMode::PerThread`.
3. `config/*` — `impl Default` on substructs (e.g. `ScanOptions::default`
   re-hardcodes `page_size: 4096`), the `DEFAULT_CONFIG` /
   `default_extract_config` merge baselines, and the umbrella logic
   (`capture_all = !no_capture_all_metadata` / `apply_all = apply_metadata` +
   per-bit `resolve_bool_flag`).

The same value is often written 2–3 times; the umbrella *base direction*
(archive = capture-all INCLUDE, extract = apply-nothing EXCLUDE) is implicit in
`try_from`.

## Design — two buckets

### A. General defaults → single source
- Bools: delete every `default_value_t = false` on `SetTrue` args
  (behavior-neutral; clap already defaults absent flags to `false`). The CLI
  defines no bool defaults; the config does.
- Numerics: add named consts to `defaults.rs` (`DEFAULT_PAGE_SIZE = 4096`,
  `DEFAULT_MIN_PAGES = 0`, optionally `DEFAULT_STRIP_COMPONENTS = 0`),
  referenced from both `cli.rs` and the config `Default` impls
  (`ScanOptions::default`, new `SparseOptions::default`).
- Enums: point `cli.rs` at each enum's own `Default`
  (`default_value_t = DedupMode::default()`, `ProgressMode::default()`, …) and
  add `impl Default` where missing (`HardLinkGrouping → Global`). The enum
  `Default` becomes the single source; `defaults.rs` stays primitive-only
  (avoids a `common → cli` dependency cycle).
- Config baselines: `default_extract_config` / `default_archive_config` become
  pure `substruct::default()` compositions.

### B. Special (umbrella) defaults → declarative group + shared resolver
- Centralize the two base directions as consts in `defaults.rs`:
  `CAPTURE_METADATA_BASE = true` (archive INCLUDE) and
  `APPLY_METADATA_BASE = false` (extract EXCLUDE).
- Keep `resolve_bool_flag` (`config/common.rs`) as the single resolver;
  `try_from` drives it from the base const instead of the inline
  `!no_capture_all_metadata` / `apply_metadata` literals.
- Optional deeper step: a `MetadataBit` table
  (`&[("xattrs", "no-xattrs"), …]` per side) + one small macro/resolver that
  expands the per-bit calls, collapsing ~20 lines of `resolve_bool_flag` in
  each `try_from`.

### Side fix (latent merge bug)
Archive `DEFAULT_CONFIG.sparse` uses `page_size: 0` (no `SparseOptions::default`
exists). Under validate-after-merge semantics that makes `--page-size` *always*
count as "set" during a reset-merge. Give `SparseOptions` a `Default`
(`page_size = defaults::DEFAULT_PAGE_SIZE`) and use it in the baseline.

## Open questions
1. Umbrella depth: minimal (base consts + inline `resolve_bool_flag`) vs full
   (bit tables + macro collapsing per-bit calls).
2. OK to delete all ~60 `default_value_t = false` (behavior-neutral)?
3. Include the `SparseOptions` baseline fix (`page_size: 0 → 4096`)?
4. Style: named consts vs a bundled `ToolDefaults` struct read field-by-field.