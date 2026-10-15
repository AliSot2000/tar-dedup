# Plan: Shared test harness consolidation + phase-by-phase tests

Status: **agreed (2026-10-14); implementation round.**

Scope: a shared `#[cfg(test)]` test-support module consolidating the duplicated test
worlds/config builders, then phase-by-phase tests for `inventory`, `place_prologue`,
`place`, `permissions`. Per-user rule: the harness/foundation is built here; the
per-phase **test lists are supplied by the user**, not invented by the builder. No
production-code behavior changes (test-only refactor).

## Why

The four TODO items (`finish testing for [inventory]`, `[place_prologue]`, `[place]`,
`[permissions]`) currently have essentially no phase-level coverage:

| Phase | File | Tests today |
|-------|------|-------------|
| inventory (archive) | `archive/inventory.rs` | 0 |
| place_prologue (extract) | `unarchive/place_prologue.rs` | 3, all on `strip_relative_member`/`transform_then_strip_order` helpers |
| place (extract) | `unarchive/place.rs` | 3, all on `annotate_copy_error` |
| permissions (extract) | `unarchive/permissions.rs` | 0 |

No test anywhere calls `inventory::run` / `place_prologue::run` / `place::run` /
`permissions::run`.

Meanwhile the archive phase tests each define their own world, so the harness is
currently duplicated 5×:

- `TestWorld { dir, db, shutdown, progress, config }` + `new()`, `rt()`, `path()`,
  `add_file()`, `add_file_recorded()`, `insert_recorded()` — near-identical in
  `archive/{hash,stage,dedup,sparsify,tar_builder}.rs`.
- `test_archive_config()` — 5 copies (same literal + per-phase overrides).

And the extract phases would each need a scan-built DB, for which the builders only
exist privately inside `unarchive/scan.rs`'s test module (`write_catalog`,
`insert`, `append_member`, `seed_filter_reasons`, `add_archived_canonical`,
`build_archive`).

## Foundation: `test_support` module

New file `crates/tar-dedup/src/test_support.rs`, registered in `lib.rs` as
`#[cfg(test)] mod test_support;`. Compile-gated support inside the crate (NOT a
`tests/` integration folder — consistent with the "tests live inline" convention;
the old `tests/` dir was deleted 2026-10). `pub(crate)` items so any module's
`#[cfg(test)]` can use them.

Consolidates:

1. **`TestWorld` (archive)** — the 5 duplicates into one struct, fields
   `dir/db/shutdown/progress/config`; methods `new()` (incl. the internal
   `filter_reason_archive` seed row, `work_dir` = `astage`), `rt()`, `path()`,
   `add_file()`, `add_file_recorded()`, `insert_recorded()`. Phase-specific
   additions (e.g. stage's `create_dir_all(work_dir)`) become parameters or stay
   local where they are genuinely phase-specific.
2. **`test_archive_config()`** — one builder replacing the 5 copies; the common
   literal plus the shared overrides the phases already agree on (paths under the
   tempdir, `jobs/io_jobs`, `sparse.page_size`, `filter.eager_filter` default).
3. **Extract harness** — move scan.rs's archive-builders into the module; expose a
   helper such as `scan_build_world(dir) -> (ExtractConfig, Database)` that writes a
   tiny tar (catalog + payload members) and runs `unarchive::scan::run`, returning
   a DB ready for `place_prologue`/`place`/`permissions`. Keeps `for_scan_test` /
   `for_place_test` config builders where they are (config-side, already shared via
   `ExtractConfig`).
4. **Probe helpers** for xattr/ACL/SELinux support stay in `common/xattr.rs` tests
   (already probe-gated; no move needed).

Existing phase tests keep their assertions; they only `use` the shared world instead
of defining their own. Any helper that is genuinely single-phase-specific stays in
its module (e.g. scan's `archive_with_partial_snapshot` variants stay local unless a
later phase needs them).

## Migration order

1. Add `test_support.rs` (archive `TestWorld` + `test_archive_config`).
2. Migrate the 5 archive phase test modules onto it; `cargo test -p tar-dedup --lib`
   green (proves the consolidation didn't change behavior).
3. Move the extract archive-builders into `test_support`; add `scan_build_world`.
   Existing scan tests keep passing.
4. Phase-by-phase tests — **user supplies the per-phase test list** for
   inventory → place_prologue → place → permissions.

## Gating for root/SELinux cases

Reuse the probe-skip pattern from `common/xattr.rs` (`TAR_DEDUP_TEST_DIR` env
override + support probe; on `ENOTSUP`/`EOPNOTSUPP`/`EPERM` print `skipped
(unsupported): …` and return). No `#[ignore]` bookkeeping. Permissions'
xattr/ACL/SELinux restore cases use this so they auto-run on the SELinux VM and skip
elsewhere.

## Build / test

- `nix develop -c cargo test -p tar-dedup --lib` (dev profile only; never `--release`).
- Targeted: `cargo test -p tar-dedup --lib <module>`.
- VM battery (user runs, builder analyzes):
  `cargo test -p tar-dedup --lib common::xattr` as normal user, and
  `sudo TAR_DEDUP_TEST_DIR=/var/tmp/tar-dedup-sel cargo test -p tar-dedup --lib common::xattr`
  as root (labelled real fs — `/tmp` is tmpfs and cannot hold SELinux labels).

## Gotchas

- `TestWorld` borrows (holds `config/db/shutdown/progress`); arrange so the world
  outlives each `run`/phase call (owned by the test fn), as the existing worlds do.
- The `filter_reason_archive` internal include row (`id=-1`) is required before any
  `apply_no_filter`/phase run (FK); fold into `TestWorld::new`.
- Hash/sparsify interrupt tests use thread bars and shutdown triggers — keep those
  additions on the phase test modules, not the shared world.
- `NewFileRecord` now carries `abs_path_raw`/`link_dst_raw`; `insert_recorded` must
  set them (`None` → stored from `abs_path`) exactly as the current worlds do.
- Do not reformat existing test code beyond the mechanical migration.
