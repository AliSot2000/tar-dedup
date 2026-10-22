# Integration-testing ideas (cross-pipeline / end-to-end)

Living notes for tests that span more than one phase or pipeline (archive → extract, or
unit-testable only through a full run). Not a numbered plan; add to this file as ideas
surface. Actual tests live inline in module `#[cfg(test)]` blocks — see
`plans/050-place-prologue-tests.md` for the unit-level coverage that complements these.

## Out-tree fan-out / source overlap

- **`--no-strict-separation` ↔ out_tree children**: with strict separation a single
  `files` row belongs to exactly one `source`, so `populate_out_tree_rel` mints exactly
  one out_tree child for it and `ref_out` has exactly one row. Under
  `--no-strict-separation` the same file is referenced by multiple `source`s → the rel
  builder maps it once per covering source → multiple out_tree rows (+ multiple
  `ref_out`). Verify the fan-out and the `INSERT OR IGNORE` collision behavior (same
  rel target dedupes).
- **`mark_source_canonical` overlap order-dependence**: the doc-comment example in
  `db/place.rs` — source A covers `/path/to/dir`, source B covers
  `/path/to/dir/subdir`, with links across both. If A is marked before B, `link_a`,
  `link_b`, `link_c` are one group; if B first, `link_b`/`link_c` group and `link_a` is
  a separate non-linked file. Needs a real inventory + extract run.

## Path mapping / escape safety

- **Transform-injected `..`**: an extract transform that rewrites a member to include
  `../` must stay clamped under `<ed>` (out_tree re-clamp in
  `build_new_out_tree_rows`). Round-trip: archive normally, extract with
  `--transform 's,^a/../,x/,'` (or similar), assert nothing lands outside `<ed>`.
- **Relative `--input-dir ../..`**: archive with a `..`-heavy original path, extract →
  everything maps under `<ed>`, no traversal.
- **Absolute `--input-dir` / `/` root**: archive with `/` and with an absolute dir;
  extract → all lands under `<ed>` (leading `/` dropped).
- **`--strip-components` + `--transform` ordering**: GNU order (transform first, then
  strip) observable end-to-end on real member names; a transform anchored on the source
  prefix must see the full relative target.
- **Empty-name skip sentinel**: a transform (or strip) that reduces a member to `""`
  must skip the row in the out_tree (GNU "transforms to empty name" behavior) — verify
  the file is absent from the extraction tree.

## Placement behavior

- **`--link-tree`**: canonical election is skipped in link-tree mode — out_tree
  `canonical_id` stays NULL end-to-end.
- **`ensure_parent` in a real `place` run**: `--files-from` listing a file under a
  directory not covered by any recursive source — `populate_out_tree`/`ensure_parent`
  must materialize the missing parents so `place` can create the file.
- **Hardlink canonical election Global vs Source** across a real multi-source tree with
  real same-inode files (see overlap case above).
- **Resume idempotency of placement_prologue**: interrupt after prologue, resume →
  `placement_prologue_done` short-circuits; out_tree not duplicated.
