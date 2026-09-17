# Plan: rework CLI metadata flags (companion pairs + umbrella presets)

## Problem
clap_derive flattens every `#[arg]` attribute on a field into **one** builder method
list. Duplicate `.long()` / `.action()` calls are last-wins, so a single field can never
expose two different-action flags. The previous `#[arg]` + `#[arg]` stacking silently
produced only the last flag (e.g. `--null` + `--no-null` became just `--no-null`).
Fix: one field = one flag; paired flags are **separate SetTrue fields**; umbrella
"all metadata" states are **single-sided presets** selected by one flag each.

## Flag model (both pipelines)
- Every pair member is its own `bool` field with `ArgAction::SetTrue`, default `false`.
- Per-bit resolution, explicit flag wins over umbrella, umbrella wins over default:
  `no_X` -> false · `X` -> true · neither -> umbrella? (include preset / exclude preset).
- Umbrella flags are SetTrue and select the base preset for a fresh run. `build(b)`
  picks the preset when no stored base is passed; stored base (resume/future reset)
  keeps its own concrete values and the umbrellas just sit as ordinary bits there.

## Archive (`ArchiveArgs`)
- **Default = everything ON** (INCLUDE preset). Drop `--capture-all-metadata`
  (redundant). Only umbrella: **`--no-capture-all-metadata`** (EXCLUDE preset).
- File Attributes group:
  - `acls`/`no_acls`, `xattrs`/`no_xattrs`, `selinux`/`no_selinux` pairs
    (positive fields flip `default_value_t = true` -> `false`).
  - `numeric_ids_only` moved into File Attributes with counter
    `resolve_numeric_ids` (`--numeric-ids-only` / `--resolve-numeric-ids`;
    user decision) — NOT part of the umbrella base, default resolve names.
  - `no_capture_all_metadata` (`--no-capture-all-metadata`), new field.
- Remaining stacked pairs collapse to their single real flag (`--null`,
  `--no-recursion`, `--dereference`, `--one-file-system`, `--no-hardlink-detection`,
  `--no-strict-separation`, `--exclude-vcs`, `--anchored`, `--ignore-case`,
  `--sparsify`, `--fail-fast`, `--no-errors`, `--no-dedup`, `--lazy-filter`
  (fix `lazy_filter` underscore typo)).

## Extract (`ExtractArgs`)
- **Default = everything OFF** (EXCLUDE preset). Drop `--no-apply-metadata`.
  Umbrella renamed: **`--apply-all-metadata`** (was `--apply-metadata`).
- File Attributes pairs (all default false):
  `restore_owner`/`no_same_owner`, `apply_stored_owner_map`/`no-…`,
  `apply_stored_group_map`/`no-…`, `apply_mode`/`no_apply_mode`,
  `apply_transform`/`no_apply_transform` (NEW), `apply_atime`/`no_apply_atime`,
  `apply_mtime`/`no_apply_mtime`, NEW positives `xattrs`, `acls`, `selinux`,
  `same_permissions` (join existing `no_*`).
- Remaining stacked pairs collapse to single real flag (`--absolute`/`-P`/
  `--absolute-names`, `--unlink-first`/`-U`, `--validate-maps`, `--fail-fast`,
  `--no-errors`, `--anchored`, `--ignore-case`, `--lazy-filter`).

## `same_owner` — the inference exception
- `--same-owner` -> definitely apply (`true`).
- `--no-same-owner` -> definitely not (`false`).
- neither + INCLUDE base (`--apply-all-metadata`) -> `infer_same_owner()`
  (euid is root => true).
- neither + EXCLUDE base -> `false` (== `--no-same-owner`).
- `attributes.restore_owner` mirrored to the same value.

## Config (`config/{archive,extract}.rs`)
- `build(args, base)`: ① validate pair conflicts (any `X && no_X` -> `Error::Config`)
  → ② base = `Some(base)` else umbrella preset (INCLUDE/EXCLUDE) → ③ build fields,
  resolving every bit via the explicit-beats-umbrella table → ④ policy enums
  (`mode_policy`/`transform_policy`/`owner_policy`) derived from resolved bits.
- `ArchiveConfig`: move `numeric_ids_only` from `ArchivePipelineOptions` ->
  `CaptureOptions`; update `archive/inventory.rs:108` ->
  `config.capture.numeric_ids_only`.
- `main.rs`: `try_from(&args)` -> `build(&args, None)` for Archive/Extract arms.
- Extend `merge_over`/`DEFAULT_CONFIG` baselines so fresh builds equal the presets.

## Files & changes
| # | File | Change |
|---|------|--------|
| 1 | `plans/cli-metadata-flags.md` | this plan |
| 2 | `AGENTS.md` | plan-to-`plans/` workflow note |
| 3 | `cli.rs` | all pairs as SetTrue-SetTrue fields; umbrellas; collapsed singles |
| 4 | `config/archive.rs` | presets, bit resolution, validation, numeric move |
| 5 | `config/extract.rs` | presets, bit resolution, validation, same-owner inference |
| 6 | `archive/inventory.rs` | `config.pipeline.numeric_ids_only` -> `config.capture.numeric_ids_only` |
| 7 | `crates/tar-dedup-cli/src/main.rs` | `try_from` -> `build(&args, None)` |

## Verification
- `cargo check` green (excluding user WIP `inventory.rs`/`permissions.rs` errors already present).
- Spot checks: `--apply-all-metadata --no-apply-mtime`, `--no-capture-all-metadata --acls`,
  `--acls --no-acls` errors, bare archive captures all, `--numeric-ids-only` +
  `--resolve-numeric-ids` counter, `-P`/`--null`/`--lazy-filter` restored.