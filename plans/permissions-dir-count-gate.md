# Plan: permissions-phase dir progress-count gate (`force_overwrite_dir` follow-up)

Status: **agreed (2026-10-12), implementation round.** Follow-up to
`plans/force-overwrite-dir.md` (population side already done).

## Problem

The permissions phase keys its dir *progress count* and its dir *apply* on
different flags:

```
let (dir_pending, dir_done) = if force_overwrite_dir { count_dirs() } else { (0,0) }  // :65
...
if !no_overwrite_dir { process_batches(..., dirs=true) }                              // :81
```

- **Default (no flags):** dirs are applied (`!no_overwrite_dir` true) but the count
  is `(0, 0)` -> the progress bar omits all directory work.
- **Both flags set:** count runs (force true) but apply is skipped -> count != apply.
- The apply path itself is already correct in both modes: `list_out_tree_for_permissions_dirs`
  LEFT-JOINs `files` and applies metadata to every `out_tree` dir row with a `file_id` —
  in non-force mode `out_tree` only contains filter-selected dirs (filter applied at
  population in `place_prologue`), in force mode it contains all dirs (incl. filter-excluded).
  `ensure_parent` NULL-`file_id` ancestors are skipped by the join.

## Change

### `unarchive/permissions.rs:64-69`

Match the apply gate; delete the `// TODO: Fix - this is not correct.` comment:

```rust
let (dir_pending, dir_done) = if !rt.config.attributes.no_overwrite_dir {
    rt.db.count_out_tree_for_permissions_dirs()?
} else {
    (0, 0)
};
```

Precedent: `db/integrity.rs:697` already uses `apply_dirs = !no_overwrite_dir`.

### `unarchive/permissions.rs:197`

Remove the stale `// TODO: bool to only return directories which are not excluded.` —
the filter distinction is encoded in `out_tree` at population time; adding a filter
here would break `--overwrite-dir` (excluded dirs must still get metadata).

## Verification

- `cargo build -p tar-dedup -p tar-dedup-cli`
- `cargo test -p tar-dedup --lib`
