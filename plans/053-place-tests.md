# Plan: place tests (unarchive/place.rs)

Status: **agreed (2026-10-22).** Comprehensive test battery for `place.rs` —
the extraction materializer. Today `place.rs` has only three tests
(`annotate_path`, covered). This adds unit tests per helper, loop/pass tests
for the three materialize passes and `run()`, and a hardlink-group integration
battery. The `get_file_times` error arm is deferred to a mock seam (see
`to-mock.md`).

## Phase 0 — shared extract test world (refactor first)

`scan.rs`, `rehash.rs` and `place_prologue.rs` test modules share a near-identical
skeleton (`tempdir + Database + Shutdown::detached + ProgressBarSet +
ExtractConfig + rt/rt_with/path/seed_filter`). Consolidate before adding a third:

- Add `ExtractWorld` to `test_support.rs`:
  - fields `{ dir: TempDir, db: Database, shutdown: Shutdown, progress: ProgressBarSet, config: ExtractConfig }`
  - `pub fn new(config: ExtractConfig) -> Self` — creates `config.paths.work_dir`, opens `db_path()`, detached shutdown, `ProgressBarSet::new(EXTRACT_MULTIPLIER)`
  - `rt()`, `rt_with(shutdown)`, `path(name)` (join under `dir.path()`), `seed_filter()` (joint `-1` archive+extract include rules)
- Rebase `rehash::ExtractTestWorld` and `place_prologue::PrologueWorld` onto it;
  keep module-specific seeders as local `impl ExtractWorld` blocks (allowed: the
  type is crate-local).
- `place.rs::tests` uses the shared world + local `PlaceWorld` seeders.

`PlaceWorld` local seeders:
- `add_file(abs, size, sha, ftype, canonical)` → `insert_file` + `mark_self_canonical`/`set_canonical` + `update_file_inspection_per_id`
- `add_out(abs, file_id, flags, canonical_id)` → `insert_out_tree_rows` + raw-SQL canonical_id
- `put_payload(content_id, bytes)` → write into `paths.extract_cache_dir()` (== work_dir)
- `mk_stripped` / `mk_record` builders; `mk_cid(sha, size, id)` via `content_id_from_digest`

## Phase 1 — unit tests

### build_path
- missing path → created; existing dirs untouched.
- `already_checked` ends == target.
- non-dir segment: `remove_and_replace=false` → `Err(Config)`; `true` → replaced, continue.
- symlink segment: `keep_dir_symlink=false` → removed + dir created; `true` → left alone.
- too-high assert (line 1001 `prefix.len() >= extraction_root.len()`): sibling roots
  whose common prefix is shallower than the root → `catch_unwind`.
- recorded-error arm: read-only parent → `create_dir_all` fails → recorder buffer +1, `Err`.

### check_path
- not exists → `(true,false,false)`.
- `symlink_metadata` error (parent `chmod 0o000`, unix): silent → `(false,true,false)`; not → `Err(Io)`.
- db_mtime `None` → `(false,true,false)` (both silent modes; arm never errors).
- policy × silent × a<b / a=b / a>b matrix:
  - Replace → always proceeds.
  - PreferNewer → proceed iff `db > live`; `<=` keeps (on-disk wins on tie).
  - PreferOlder → proceed iff `db < live`; `>=` keeps (on-disk wins on tie).
  - silent → `Ok((false,true,false))`; not → `Err(General)`.
- file-type mismatch (live symlink vs rec File): `remove_and_replace=true` → `(true,true,true)`;
  false → silent `(false,true,false)` / not `Err(General)`.
- broken symlink → `determine_file_type` Err → silent proceeds (Unknown) / not `Err(Io)`.
- remove/unlink_first: `unlink_first=true` → `(true,true,true)`; remove fails (dir) →
  silent `(false,true,true)` / not `Err(Io)`; no-remove arm → `(true,true,false)`.
- `get_file_times` Err arm → **deferred, needs mock** (see `to-mock.md`).

### copy_single_file
- `no_reflink=true` → copy, `(fid, true)`.
- reflink enabled → `Ok`, dst content == src (reflink or fallback).
- force + no_reflink → `Err((fid, Interrupted))`.
- src missing → `Err((fid, FileStat))`, no leftover dst.

### copy_one_to_source
- copy, `is_copy=true`; cache removed (keep_stage false) / kept (true).
- `no_reflink=false` → reflink or fallback; dst content == src.
- force pre-set + no_reflink → `None` (deterministic `Interrupted` arm).
- src missing → `Err((id, Error))`, dst removed.

### build_other
- File / Dir / Unknown → `catch_unwind` panic.
- Socket → `Ok(())`, nothing created.
- Symlink: `link_dst=Some` → symlink (`read_link`); `None` → `Ok`.
- FIFO: `try_special=true` → fifo created; `false` → `Ok`.
- Block/Char dev: `try_special=true` + major+minor → `mknod` (verify `rdev`);
  missing major / minor / both → `Err(General)`; `try_special=false` → `Ok`.

### relative_pardirs_to_dir
- depth `../../`, same-dir `.`; `catch_unwind` for sibling, file==dir, above root.

### materialize_one (per kind)
- Files: no target → placed; policy conflict → `placed=false`; copy error → `Err`; force pre-set + no_reflink → `None`.
- Hardlinks: dst free → hardlink Ok; conflict → skip; missing `out_canon` → `Err`.
- Others: symlink → placed; build_other error → `Err`.
- Assert `MaterializeResult` fields.

### process_results
- Ok + Err(FileStat) mix → flags set, `recorder.buffered() == n_err`.
- `Err(Interrupted)` → no flag, no record.
- `Err(other)` → `catch_unwind` panic.

### copy_canonical_worker / materialize_worker (channel)
- dropped work queue → sends `None`.
- graceful before work → breaks → `None`.
- happy → `Some(...)` then `None` on sender drop / exit.
- (force-mid-copy `None` arm tested at function level, not in worker.)

### materialize_link_tree
- absolute link target (`absolute_links=true`) vs relative (`false` resolves).
- `use_hard_links=true` → same inode as source.
- pre-existing out path → `Err(Config)` "Link Tree must be empty".
- Unknown/Dir asserts (301-302): unreachable via `list_out_tree_for_linking` filter — verify via lister query, not the assert.

### copy_canonicals_to_source
- happy: payloads at `{root}/{link_source}/`, cache removed (keep_stage false), `AtLinkSource` set.
- custom `link_source` name.
- `pending == 0` → queue dropped, no-op.

## Phase 2 — loop / pass tests

### prepare_extraction_dir
- happy: dirs created on disk, `dir_tree_is_built` true.
- `request_graceful` mid-loop → `Err(Interrupted)`.

### materialize_loop (files/hardlinks/others)
- full 3-pass: 1 canonical (payload in cache) + 1 hardlink row (canonical out placed on disk) + 1 symlink row → placed/hardlink/symlink, flags `Placed`/`IsHardlink`/`UsedRefLink`.
- `pull_materialize_queue` returns size-DESC first.
- graceful → `Err(Interrupted)`.

### run
- clean_target + one_top_level: file removed, `root/sub` created.
- clean_target with `one_top_level=None` → nothing deleted.
- prepare_extraction_dir no-op when `dir_tree_is_built`.
- **one link_tree smoke**: 1 canonical + 1 dup → symlink created, cache removed, `Placed` flags, phase promoted.

### cleanup_stage (unit — covers run's remove-extract-cache fail-fast)
- `work_dir` as regular file, `fail_fast=true` → `Err(Config)`; `false` → `Ok` (warn).

## Phase 3 — integration battery (hardlink groups)

Hand-rolled DB + real disk hardlinks (no archive needed — grouping is SQL over
`files(dev,inode,canonical_id)` + `out_tree` + `ref_out`; materialize_hardlinks
only needs the canonical `out_tree.abs_path` on disk):
- partial overlap `/a/b/c` + `/a/b`: `c/` forms one group; `/a/b` minus `c/` a second. `/a/b` then `/a/b/c` → one group, not two (both orders).
- dir `d/` + symlink `sa/→d/`: `HardLinkGrouping::None` → no cross links; `Source` → links within each source only; `Global` → `d/` and `sa/` share.

## Deliberately skipped (annotated)

- `annotate_path` — already covered (3 existing tests).
- `check_in_flight` idempotence — covered by `shutdown.rs` tests.
- link_tree Unknown/Dir assert — unreachable via the lister.
- `get_file_times` Err arm — needs mock seam, tracked in `to-mock.md`.
- mid-copy force race — replaced by pre-set-force function-level tests.

## Files touched

- `to-mock.md` (new; get_file_times entry)
- `crates/tar-dedup/src/test_support.rs` (ExtractWorld)
- `crates/tar-dedup/src/unarchive/rehash.rs` (rebase world)
- `crates/tar-dedup/src/unarchive/place_prologue.rs` (rebase world)
- `crates/tar-dedup/src/unarchive/place.rs` (new `// TEST` battery)
- `plans/053-place-tests.md`

## Verification

- `cargo test -p tar-dedup --lib` (default suite; pre-existing blocker only)
- `cargo test -p tar-dedup --lib --features test-all` (full, incl. compression battery)

## Implementation notes (2026-10-23)

Implemented. Default suite 554 passed / 1 failed (pre-existing blocker
`clear_archive_meta_drops_only_byte_counters`); `test-all` 563 passed / 1
failed. The battery surfaced **four latent production bugs**, all fixed:

1. `db/place.rs list_out_tree_for_linking` — SQL referenced an undefined alias
   `f` (should be `ent`): `--link-tree` extract errored at runtime ("no such
   column: f.ftype"). Fixed to `ent`.
2. `unarchive/place.rs materialize_loop` Hardlinks pull — the positional cursor
   advanced to `entries[..].1.id` (the *canonical* out row) instead of `.2.id`
   (the *entry* out row). Since a group's canonical is `MIN(out.id)`, the next
   `o.id > last_id` re-returned the same row forever → `materialize_hardlinks`
   hung for Global/Source grouping. Fixed to `.2.id`.
3. `db/place.rs files_to_move_where` — `" AND {}" + filter` emitted a literal
   `{}` into SQL (should have been `format!`), so `count_files_to_move(true)` /
   `count_moved_files(true)` failed → `copy_canonicals_to_source` errored for
   every `--link-tree` run. Fixed.
4. `db/common.rs list_out_tree` — the `only_dir` filter bound `:tgt = 1` but
   `IsDirectory.mask_i64()` is `1<<8 = 256`, so `flags & 256 = 1` never matched
   → `only_dir=true` returned nothing → `prepare_extraction_dir` never built the
   dir tree on extract. Fixed to bind the mask.

### Deviations from the agreed design (test-side)

- **ExtractWorld constructor**: `with_config(make_config)` instead of the
  planned `new(config)` — `new(config)` cannot own the tempdir the config points
  into (rehash's zero-arg `ExtractWorld::scan_test()` needs the world to own it,
  else the work dir vanishes). `scan_test()` lives on the shared world;
  `rehash.rs`, `place_prologue.rs` and `filter.rs` all rebased onto it.
- **check_path `symlink_metadata`-error and `determine_file_type`-error arms are
  unreachable from a test**: `tgt.exists()` short-circuits to `(true,false,false)`
  for any inaccessible/broken target, so the later error arms can only be hit via
  a TOCTOU race. Dropped; the reachable type-mismatch path (symlink→file) is
  covered. Documented in `to-mock.md` alongside `get_file_times`.
- **run() link_tree smoke** uses one canonical row: dups carry no `FileExtracted`
  (only canonical payloads are unpacked), so `list_out_tree_for_linking` (which
  requires `AtLinkSource` on the entry) never links dup rows — asserted the
  canonical-only behavior.
- **prepare_extraction_dir interrupt** uses `request_force`, not graceful
  (`check_in_flight` only reacts to force; the function's own log line says it
  "cannot be gracefully interrupted").
- **build_other char-device** guarded for unprivileged runs (mknod → EPERM).
- materialize-one conflict tests set `silent_conflicts = true` so the "keep
  existing" outcome is the `Ok(placed=false)` path, not the loud `Err(General)`.
- `link_tree_existing_path_errors` hits the intended `Error::Config` because
  `materialize_link_tree` collects per-row errors via the `results` Mutex and
  only the lister-side `Ok`-propagation error surfaces as `Err`.

