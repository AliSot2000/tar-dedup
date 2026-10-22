# Plan: `place_prologue` unit-test battery (+ `db/place.rs` canonical-election tests)

Status: **agreed (2026-10-22).** Unit-test the pure-DB placement prologue
(`crates/tar-dedup/src/unarchive/place_prologue.rs`). All functions operate on data
(DB rows / in-memory structs), never on the filesystem, so they are cheaply testable
with a real in-memory/tempdir `Database`. Also add the missing `mark_*_canonical`
election tests in `db/place.rs` (no `#[cfg(test)]` module exists there yet).

## Scope

- `place_prologue.rs` `#[cfg(test)]` module: extend the existing helpers tests with a
  local `PrologueWorld` harness (tempdir + DB + `ExtractConfig` + progress + shutdown)
  and per-function tests listed below.
- `db/place.rs`: new `#[cfg(test)]` module with `mark_global_canonical` /
  `mark_source_canonical` SQL-election tests (raw-SQL seeding, in-memory conn).
- `integration-testing-ideas.md` at repo root: living notes doc for cross-pipeline
  integration-testing ideas (strict separation fan-out, transform/escape, ordering,
  `--link-tree`, `ensure_parent`, empty-name skip, mark-source overlap order).

## Harness

New local `PrologueWorld` in `place_prologue.rs::tests` (mirrors archive `TestWorld`,
extract side does not have one):

```
struct PrologueWorld {
    dir: tempfile::TempDir,
    db: Database,
    shutdown: Shutdown,
    progress: ProgressBarSet,
    config: ExtractConfig,   // ExtractConfig::for_place_test(root, absolute_names, jobs), mutatable
}
impl PrologueWorld {
    fn new(root: &Path, absolute_names: bool, tune: impl FnOnce(&mut ExtractConfig)) -> Self
    fn rt(&self) -> ExtractRTArgs<'_>
}
```

Seed helpers:
- source: `db.add_get_source(abs, "test", None, original, abs, original,
  SourceFlags::IsDirectory)`
- file+ref: `db.insert_file_and_ref(source_id, NewFileRecord { ftype: Some(...), .. })`
- filter gate: `test_support::seed_filter_reasons(&db, id)`
- raw-SQL rows (bypass `insert_file`'s `debug_assert!(is_absolute)`): `db.with_transaction`
  + `INSERT INTO files (id, abs_path, abs_path_raw, ftype, ext, size,
  include_reason_archive, include_reason_extract) VALUES (...)`, mirroring
  `db/place_prologue.rs` test `insert_row`.

## Tests — `place_prologue.rs`

### `resolve_transform` (all 4 arms)
- `TransformSource::None` → `Ok(None)`
- `Cli("s,a,b,")` → `Ok(Some(_))`
- `Cli("garbage")` → `Err(Config)`
- `Stored` after `set_archive_transform("s,a,b,")` → `Ok(Some(_))`; `Stored` with no meta
  → `Ok(None)`

### `populate_new_names` (debug-assert trips, `catch_unwind`)
Feasible via raw-SQL rows; `insert_file` would assert at insert time, raw SQL bypasses it.
`absolute_names = true`, `strip = 0`:
- raw-insert `abs_path = "relative/x"` → `abs_rel_path` `debug_assert!(is_absolute)` fires
- raw-insert `abs_path = "/a/./b"` (non-normalized) → `debug_assert_eq!(abs.clean(), abs)` fires
Both wrapped in `AssertUnwindSafe` + `catch_unwind`, assert `is_err()`.

### `strip_relative_member` (wrong-path panics)
- `"/a/b"` → RootDir arm → panic `"Unexpected path component root-dir"`
- `"a/../b"` → ParentDir → panic `"parent-dir"`
- `"a/./b"` → CurDir → panic `"current-dir"`
Existing no-op (`strip=0`) and over-strip (`strip > len` → `""`) tests stay.

### `apply_renames` (DB write)
Seed 2 files (`/a/x/one.txt`, `/a/x/two.txt`), fetch `StrippedRecord`s via
`list_materialized_entries`, then:
- `transform=None, strip=0` → `new_name == "a/x/one.txt"` (converter `abs_rel_path`)
- `transform=Some(parse("s,^a,b,")), strip=1` → GNU order: transform then strip →
  `"x/two.txt"`
Assert via `get_file_by_id::<StrippedRecord>().new_name`.

### `populate_out_tree_abs` (rel table left empty)
Seed source + files (abs paths), `absolute_names = true`, call
`populate_out_tree_abs(rt, false)`:
- `count_out_tree_rows() > 0`; rows at `root.join("a/b.txt")`
- `ref_out` empty: raw `SELECT COUNT(*) FROM ref_out` == 0

### `populate_out_tree_rel` (rel table populated, clamped)
Seed source with `original_path = "../../Movies"`, file
`/home/user/Movies/Best-Movie-Ever.mp4`, `absolute_names = false`, call
`populate_out_tree_rel(rt, false)`:
- out_tree path == `root.join("Movies/Best-Movie-Ever.mp4")` (no `..`)
- `ref_out` count == number of out_tree rows (one child per row)

### `populate_out_tree` / `ensure_parent`
Lone-file `--files-from` shape: one file `/x/y/z.txt`, no dir rows, call
`populate_out_tree(rt, false)`:
- every non-dir out_tree row's parent exists as a dir out_tree row
  (`/ed/x/y/z.txt` → `/ed/x/y`, `/ed/x` present)

### `build_new_out_tree_rows` (pure)
Helper `mk_stripped(id, abs_path, ftype, new_name) -> StrippedRecord`.
- Mangled `new_name` with `use_new_name = true`: `".././../evil.txt"` → `root/evil.txt`,
  `"/etc/passwd"` → `root/etc/passwd`, `"../../.."` → `root`
- FileType matrix: every `FileType` variant → `OutTreeFlag::IsDirectory` set **iff**
  `FileType::Directory` (incl. every `Symlink(LinkType)` variant)

### `prepare_hardlink_canonicals` (`None` arm dispatch)
Seed files + out_tree rows (build via `build_new_out_tree_rows` + `insert_out_tree_rows`),
`config.placement.hard_link_grouping = None`, call `prepare_hardlink_canonicals(rt)`:
- every non-dir out_tree row `canonical_id == id` (raw `SELECT canonical_id` or
  `list_out_tree`)

### `stepped_dirs_then_rest` (ordering + completeness)
Seed dir + file + symlink + unknown rows, `source_id = None`, collect
`(id, ftype)` in closure:
- all dirs precede all non-dirs
- every seeded entry seen exactly once

### `run()` (smoke + idempotency)
Rel mode + `Cli` transform + `strip_components = 1`, seeded source+files:
- out_tree rows present, `ref_out` populated, `new_name` set,
  `placement_prologue_done()` true
- calling `run(rt)` again returns `Ok(())` and does not duplicate out_tree rows
  (idempotent resume)

## Tests — `db/place.rs`

New `#[cfg(test)] mod tests` with in-memory `conn()` + schema init + filter-reason
seed (same as `db/place_prologue.rs`). Raw-SQL seeding of `files` (dev/inode,
`canonical_id`, `ftype='file'`) + `out_tree` rows (+ `ref_out` for source).

### `mark_global_canonical`
Two files sharing `(dev, inode)`, dedup canonical `A` self-linked, both pointing at `A` as
content canonical; two out_tree rows (one per file). After election:
- exactly one out_tree row is self-canonical (`canonical_id = id`)
- the other points at the elected id
- returned `u64` == number of updated rows

### `mark_source_canonical`
Same shape, but one out_tree row gated under source A (`ref_out`), one under source B.
`mark_source_canonical(A)`:
- only A's group elects; B's row `canonical_id` stays NULL

## Files touched

- `crates/tar-dedup/src/unarchive/place_prologue.rs` (tests only)
- `crates/tar-dedup/src/db/place.rs` (new tests module)
- `plans/050-place-prologue-tests.md`
- `integration-testing-ideas.md` (repo root, new)

## Verification

- `cargo test -p tar-dedup --lib -- unarchive::place_prologue`
- `cargo test -p tar-dedup --lib -- db::place`
- full `cargo test -p tar-dedup --lib` (dev profile; expect the pre-existing
  `clear_archive_meta_drops_only_byte_counters` failure to remain)
