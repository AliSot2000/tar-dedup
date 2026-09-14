# Plan: `--transform` / `--xform` + `--apply-transform`

## Semantics (GNU tar, manual §6.7)
- Syntax `s/DELIM re /DELIM replace /[flags]`, any consistent delimiter, multiple
  `s` clauses joined by `;`, applied in order (each to the previous result).
- Flags: `g` (all matches), `i` (case-insens). `x` accepted as a no-op (regex-crate
  is already extended). `number` flag, non-`s` commands, and GNU scope letters
  (`r/s/h/R/S/H`) -> `Error::Config`.
- Pattern uses the existing `regex` crate (linear time, consistent with `filter.rs`).
  Pattern backrefs are unsupported (would force NFA backtracking / exponential blowup).
  Replacement is translated for the crate: `&` -> `$0`, `\1..\9` -> `$1..$9`,
  literal `$` -> `$$`, `\&` -> `&`.
- Applies to the full **member-relative** name. GNU order: **transform -> strip**.
- Empty result -> `new_name=""` -> warn + skip member.
- Applies to all members incl. dirs and symlink paths; soft-link/hard-link
  `link_dst` **targets are not rewritten**. Hard-link member *paths* are renamed
  (each member's own out_tree row).

## Store-only, mirroring `--mode`
- Archive: `--transform` validated at config; stored as meta `archive_transform`
  (raw string). **No computation at archive time.**
- Extract: `--transform` (CLI wins) vs `--apply-transform` (stored) vs none ->
  `TransformSource { None, Stored, Cli(String) }` (mirrors `ModeSource`).

## Build path
- `use_new_name = has_transform(...) || use_strip`
  (`transform.is_some() || config.strip_components > 0`).
- When `use_new_name`: `populate_new_names` sets `new_name` for every materialized
  row (identity member if unchanged, `""` if empty). The out_tree build uses a
  DB filter `AND f.new_name IS NOT NULL AND f.new_name != ''` and places each at
  `base.join(new_name)` — **no `abs_path` fallback in this mode**.
- When `!use_new_name`: build from `abs_path` as today; `new_name` ignored.

## Files & changes
| # | File | Change |
|---|------|--------|
| 1 | `common/transform.rs` (new) | `TransformSource`, `TransformExpr{steps}`, `parse_transform_expr`, `apply`, replacement translation + unit tests. |
| 2 | `cli.rs` | ArchiveArgs + ExtractArgs `--transform`/`--xform`; ExtractArgs `--apply-transform`. |
| 3 | `config/archive.rs` | `CaptureOptions.transform: Option<String>` validated via `parse_transform_expr`. |
| 4 | `db/meta.rs` + `db.rs` | `MetaKey::ArchiveTransform`="archive_transform", `MetaEntry::ArchiveTransform(String)`, get/set facades. |
| 5 | `archive.rs` | `RunFresh`: store `capture.transform` next to `set_archive_mode_changes`. |
| 6 | `config/extract.rs` | `ExtractConfig.transform_policy: TransformSource` + `resolve_transform_policy_from_args`; update ctors. |
| 7 | `db/place.rs` + `db.rs` | `list_materialized_entries` gains valid-`new_name` filter param. |
| 8 | `unarchive/place_prologue.rs` | `run()` resolves `Option<TransformExpr>`; `use_new_name`; `populate_new_names` does transform->strip and always sets `new_name`; `populate_out_tree`/`_abs/_rel`/`build_new_out_tree_rows` use `base.join(new_name)` when `use_new_name`. |
| 9 | `tests/common/mod.rs` | `place_config` `transform_policy: TransformSource::None`; adjust `populate_out_tree` helper signature. |
| 10 | tests | `common/transform.rs` parser suite; `place_prologue` transform->strip + `""` skip; `meta` round-trip for `ArchiveTransform`. |

## Deviations (document)
- Pattern backrefs dropped (linear-time regex); ERE-style not GNU BRE.
- `number` flag and GNU scope letters unsupported (names-only).
- Store-only single application (explicit extract `--transform` overrides, does not re-transform).
- Anchors target the member-relative string (rel-mode source-root-relative wart, same as strip).
