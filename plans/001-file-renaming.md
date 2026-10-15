# Plan: file name rewriting — `--strip-components` + `PlacementPrologue` refactor

## Scope (v1)

Implement GNU-tar `--strip-components=NUMBER` on **extraction only** (GNU:
*"Strip given number of leading components from file names before extraction"*,
and *"Unlike --strip-components, --transform can be used in any GNU tar
operation mode"*). The sed `--transform`/`--xform` feature is **deferred** to a
follow-up (sed-subset parser, storage-as-meta, `--apply-transform`, scope flags).

Refactor extract into two phases:
- `PlacementPrologue` — the **entire pure-database preparation** (no FS access):
  new_name population, out_tree build, ensure_parent, hardlink-canonical
  election — guarded by a new meta flag.
- `Place` — the filesystem work only (dir tree via `build_path`, clean_target,
  canonical copy, materialize/link/others, status messages, stage cleanup).

## Key design decisions

1. **Member-relative renaming.** Our DB stores absolute paths; the recursive
   target mapping rebases via `source_abs` (rel mode) or `/` (abs mode). The
   rename operates on the **relative member**:
   - abs mode: `member = abs_path.strip_prefix('/')`
   - rel mode: `member = abs_path.strip_prefix(source_abs)`

   Target = `base.join(member_or_new_name)` where base = root / source_base.
   This is what the user explicitly confirmed: **strip happens only AFTER the
   absolute path has been converted to a relative one**. Storing abs results
   would risk dropping the `source_abs` prefix and panicking in
   `catalog_to_target_abs`.

2. **Recompute-all, never chain.** `new_name` is a derived cache (`f(member,
   flags)`), recomputed from scratch from `abs_path` on every prologue run.
   The DB column is never an input to itself, so re-running / re-supplying
   `--strip-components` can never compound. build_out_tree consumes it;
   `new_name == ""` is the "empty name" skip sentinel (GNU warns + skips).

3. **Gating.** The prologue stage and the out_tree consumer only consult
   `new_name` when `config.strip_components > 0`; otherwise everything behaves
   exactly as today (no clearing needed, no staleness).

4. **Storage location.** `new_name` column already exists in the schema
   (`db/schema.rs`, "used to store name transformations") — **no schema
   migration**.

5. A new meta flag marks the prologue phase done; `place::run` depends on it
   (`debug_assert`). Both `out_tree_built` / `dir_tree_built` remain.

## Files & changes

| # | File | Change |
|---|------|--------|
| 1 | `config/phases.rs` | `ExtractPipelinePhase::PlacementPrologue` between `Rehash` and `Place`; `as_str` = `"placement_prologue"`, `parse`, `next`. |
| 2 | `unarchive.rs` | New phase arm `PlacementPrologue => place_prologue::run(...)`; `mod place_prologue;`; change test re-export to `pub use place_prologue::populate_out_tree;`. |
| 3 | `db/meta.rs` | `MetaKey::PlacementPrologueDone` = `"placement_prologue_done"`; `MetaEntry::PlacementPrologueDone(bool)`; decode via `parse_bool`. |
| 4 | `db/place.rs` | `placement_prologue_done(conn)` / `set_placement_prologue_done(conn)` beside `out_tree_built` accessors; `list_out_tree` **removed** (moved to `db/common.rs`). |
| 5 | `db/common.rs` | Receive `list_out_tree` (needs `OutTreeFlag` import); add `new_name` to `StrippedRecord::sql_columns` + `from_row`; `to_stripped()` carries `new_name`. |
| 6 | `db/types.rs` | `StrippedRecord` gains `pub new_name: Option<String>`. |
| 7 | `db.rs` | Facade `Database::list_out_tree` backend -> `common::list_out_tree`; add `placement_prologue_done()` / `set_placement_prologue_done()`. |
| 8 | `unarchive/place_prologue.rs` (new) | `populate_new_names(...)` (strip stage, recompute-all, member first then strip, `UPDATE files SET new_name`), moved `populate_out_tree` + `_abs/_rel` + `build_new_out_tree_rows` (now consumes `new_name`) + `catalog_to_target_abs` + `ensure_parent` + `prepare_hardlink_canonicals` (skip when `--link-tree`); sets `placement_prologue_done` + `set_out_tree_built`. |
| 9 | `unarchive/place.rs` | Trim to FS work: drop the `populate_out_tree` call + moved helpers + `prepare_hardlink_canonicals` call; add prologue precondition; remove now-unused imports. |
| 10 | `cli.rs` (ExtractArgs) | `--strip-components=u32` (0 = no-op). |
| 11 | `config/extract.rs` | `ExtractConfig.strip_components: u32`; wire from args; update test ctors. |
| 12 | tests | Unit: `strip_relative_member` (counts, <= n -> `""`, 0 -> None), meta round_trip for `placement_prologue_done`; update `tests/common/mod.rs:17` re-export. |

## Rules

- Pure-DB prologue funcs set the flag; place runs only FS work. `--exit-after-stage=placement_prologue` works via phase `parse`.
- Strip runs on the **relative** member (conversion first, strip second).
- No new schema; no sed parser; no `--transform` in v1.