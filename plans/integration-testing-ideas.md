# Integration-testing ideas (cross-pipeline / end-to-end)

Living notes for tests that span more than one phase or pipeline (archive → extract, or
unit-testable only through a full run). Status updated 2026-10-30 — all items
implemented via the scenario suite + in-process tests (see `060-finish-integration-tests.md`).

Legend: ✅ scenario · 🔵 unit test (in-process).

## Out-tree fan-out / source overlap

- ✅ **`--no-strict-separation` ↔ out_tree children** — `no_strict_separation_fanout`
  (end-to-end: every file present exactly once, contents correct) +
  🔵 `db::place::tests::no_strict_separation_fanout_accumulates_ref_out`
  (ref_out fan-out + `INSERT OR IGNORE` dedupe).
- 🔵 **`mark_source_canonical` overlap order-dependence**
  — `db::place::tests::mark_source_canonical_overlap_covering_source_first_groups_all`
  + `…_nested_source_first_splits` (the `db/place.rs` doc-comment matrix).
  End-to-end counterpart: `hardlink_overlap_matrix` (M1 covering-first / M2 nested-first
  / M3 `--no-recursion`) with `--hard-link-grouping source`.

## Path mapping / escape safety

- ✅ **Transform-injected `..`** — `path_clamp`.
- ✅ **Relative `--input-dir ../..`** — `paths_relative_input`.
- ✅ **Absolute `--input-dir` / `/` root** — `paths_absolute_root` (real `/` root
  deliberately skipped: would recurse the whole filesystem).
- ✅ **`--strip-components` + `--transform` ordering** — `paths_strip_transform_order`.
- ✅ **Empty-name skip sentinel** — `paths_empty_name_skip`.

## Placement behavior

- ✅ **`--link-tree`** — `link_tree` (outputs symlink into `<ed>/.sources/`).
- ⏳ **`ensure_parent` via `--files-from`** — pending (integration batch follow-up).
- ✅ **Hardlink canonical election Global vs Source** — `hardlink_global_vs_source`
  (+ `hardlink_grouping_none` sanity: `--hard-link-grouping none` ⇒ no grouping).
- ✅ **Resume idempotency of placement_prologue** — pre-existing in-process test in
  `unarchive/place_prologue.rs` (the `placement_prologue_done` gate short-circuits).

## Galleries

- `path_collision_transform`: two members collapsed onto one target by `--transform`
  (first-wins on disk, identical under `--no-errors`).
- xattr family: `xattr_user`, `xattr_binary`, `xattr_on_symlink` (skips on btrfs),
  `xattr_apply_all`, `xattr_none_default`.
- `dedup_roundtrip`, `resume_archive`, `cmd_inspect`, `cmd_stub_not_implemented`.