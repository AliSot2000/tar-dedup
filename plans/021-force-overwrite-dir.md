# Plan: `--overwrite-dir` / `force_overwrite_dir` — two-stage out_tree population

Status: **agreed (2026-10-05), implementation round.** User approved design option 4
(two-stage helper driving `list_materialized_entries`), the rest-pass SQL fix
(`!= 'dir'`, unknown rows preserved), and the unconditional two-stage split.

## Goal

Make `--overwrite-dir` (`attributes.force_overwrite_dir`) actually do what its
`cli.rs` doc says: *"Also apply archived directory metadata to directories
excluded by filters."* Today the flag only gates the permissions-phase progress
count (`permissions.rs:65`); excluded directories never get an `out_tree` row, so
their archived metadata can never be applied.

Mechanism (user-decision): switch the placement prologue population
(`populate_new_names`, `populate_out_tree_abs`, `populate_out_tree_rel`) to a
**two-stage process — directory entries first, then the remaining entries**. The
directory stage is optionally filter-aware:
- `--overwrite-dir` set → directory stage bypasses the archive+extract filters;
- flag clear → directory stage stays filter-aware (progress as-is today).

The permissions stage needs **no changes**: out_tree directory rows carry a real
`file_id`, the dir lister (`db/permissions.rs:70`) already LEFT-JOINs `files` and
skips NULL `file_id` (ensure_parent ancestors), and the archive-dir metadata gets
applied to the newly-created dir rows as it does for every other dir.

## Changes

### `db/place_prologue.rs` — `list_materialized_entries`

Signature gains a trailing `apply_filters: bool`:

```
list_materialized_entries(conn, last_id, batch_size, source_id,
                          only_dirs, only_valid_new_name, apply_filters)
```

- `apply_filters == false` → the `AND {filter_include_exclude}` fragment becomes
  `AND 1` (single SQL gate; no new params needed).
- `only_dirs` rest-bucket redefined: `Some(false)` is
  `AND f.ftype != 'dir'` — i.e. *everything that is not a directory*, preserving
  `unknown`-ftype rows that the current single-pass (`None`) materializes.
  (`Some(false)` is unused anywhere today, so no callers are affected.)
- Doc comment updated.

### `db.rs` — facade

`list_materialized_entries` passes the new `apply_filters` through (7th arg).

### `unarchive/place_prologue.rs` — two-stage drivers

Two private helpers replace the four `list_materialized_entries` batched loops:

```
stepped_materialized(rt, source_id, only_dirs, apply_filters,
                     only_valid_new_name, mut f: FnMut(&Vec<StrippedRecord>))
    -> Result<()>            // one batched_stepped_loop pass over one bucket

stepped_dirs_then_rest(rt, source_id, only_valid_new_name,
                       mut f: FnMut(&Vec<StrippedRecord>))
    -> Result<()>            // dirs (apply_filters = !force_overwrite_dir), then rest (true)
```

Rewired call sites:

| Site | Was | Now |
|------|-----|-----|
| `populate_new_names` abs | one loop, `only_dirs=None` | `stepped_dirs_then_rest(rt, None, false, apply_renames)` |
| `populate_new_names` rel | per-source loop, `only_dirs=None` | source loop wraps `stepped_dirs_then_rest(rt, Some(src.id), false, …)` |
| `populate_out_tree_abs` | one loop, `None` | `stepped_dirs_then_rest(rt, None, use_new_name, build+insert+inc)` |
| `populate_out_tree_rel` | per-source loop, `None` | source loop wraps `stepped_dirs_then_rest(rt, Some(src.id), use_new_name, build+insert+ref_out+inc)` |

Both passes run from `FileId(0)`; `INSERT OR IGNORE` + recompute-from-`abs_path`
keep the two-pass split idempotent w.r.t. resume. `build_new_out_tree_rows`
already sets `file_id: Some(r.id)` for dir rows (metadata source for
permissions). Ordering: `populate_new_names` → `populate_out_tree` unchanged;
dir rows exist before the child files are inserted, so `ensure_parent`
(NULL-`file_id` gaps) only fills ancestors not covered by a real dir row.

## Behavior notes

- Flag OFF (default): the partition is exact — same row set as the single pass,
  only out_tree insertion order changes (dirs first). Only risk with
  `--no-strict-separation`: which overlapping source wins a colliding mapped
  `abs_path` on `INSERT OR IGNORE`; cosmetic.
- Flag ON: excluded dirs get out_tree rows → `prepare_extraction_dir` creates
  them on disk and permissions applies archived metadata (the GNU-tar-equivalent
  intent). Excluded non-dir rows are still skipped.
- `--no-overwrite-dir` ∧ `--overwrite-dir` is a contradictory user error; dirs
  will be created either way. Not special-cased.

## Tests

New `#[cfg(test)] mod tests` in `db/place_prologue.rs` mirroring the `db/scan.rs`
fixture pattern (seeded `filter_reason_{archive,extract}` id −1 rows + reason
columns on `files`):

1. dirs-only bucket with `apply_filters=false` returns a filter-excluded
   directory row;
2. rest bucket with `apply_filters=true` still excludes the filtered row;
3. an `unknown`-ftype row lands in the rest bucket.

## Verification

- `cargo build -p tar-dedup -p tar-dedup-cli`
- `cargo test -p tar-dedup --lib` (expect 279 + new tests, 0 failures)