# Plan: coverage-closing (xz, cleanup, config, error, orchestrators)

Status: **agreed (2026-10-28).** Close the remaining unit-test gaps before
integration testing. Deferred to integration: `cmd/*` subcommands and
`db/stats.rs` (bin-only).

## A. `compression/xz.rs` — `#[cfg(all(test, feature = "test-compression"))]`

- `format_mib`: rounding (`0`, `1`, `1023`→`1 MiB`, `1024`→`1 MiB`,
  `1025`→`2 MiB`), apostrophe grouping (`1048576` → `1'048'576`? verify — the
  thin-space style: groups of 3 from the right).
- `resolve_xz_threads`:
  - no `memlimit` → `Ok(requested)` (requested `.max(1)`).
  - `memlimit ≥ usage` → `Ok(requested)` (no reduction).
  - `memlimit` forcing a reduction → `Ok(threads < requested)`.
  - `memlimit` too low (even with 1 thread) → `Err(Config)` containing
    "raise --memlimit-compress".
  - `requested = 0` → clamped to 1.
  - `preset` with `PRESET_EXTREME` → message mentions the `e` suffix.
- `mt_memusage`: non-decreasing in threads and preset level.
- `InterruptibleXzEncoder` (over a `Vec<u8>` writer):
  - write → `finish` → decode via `xz2::read::XzDecoder`, bytes round-trip.
  - `flush` forwards to the underlying writer.
  - `request_force` before write → `Err(io Interrupted)`.
  - `request_force` before `try_finish` → `Err(io Interrupted)`.
  - `abandon` → returns writer without a stream footer (decode of the output
    fails or is shorter).
  - underlying writer returning `0` from `write` → `Err(io::ErrorKind::WriteZero)`.
  - empty input `finish` → `Ok`, decodable.

## B. `common/cleanup.rs` — new `// TEST`; no feature gate

Drive via `TestWorld` (ArchiveConfig) / `ExtractWorld` (ExtractConfig)
(`WorkLayout`), tempdirs.

- `cleanup_workdir`:
  - keep_db + db file → moved to `{kept_db_parent(mode)}/{ts}_{stem}.sqlite`
    (parent created on demand).
  - keep_db but no db file → no move.
  - not keep_db + db file → db removed.
  - `keep_stage=true` → work dir left in place.
  - `remove_dir_all(work)` failure (make a subdir read-only, non-root) → warn +
    `Ok`.
  - work dir absent / not a directory → skipped.
- `reset_workdir`: db + temp files + work dir removed; removal failure → `Err`;
  absent pieces → `Ok`.
- `remove_temp_files`: `.lock`, `.snapshot-ingest.tmp`, `.snapshot-for-tar.sqlite`
  removed when present.
- `retained_db_path`: `{rfc3339-with-colons-replaced}_{stem}.sqlite` naming;
  `kept_db_parent` per mode (Archive → archive parent, Extract → extraction root).

## C. config validation battery — one **big parametrized** test per concern

Build args with `Cli::try_parse_from(["tar-dedup", <subcommand>, ...])` then
`try_from`/`Validate`. Tempdir + dummy archive file where required.

- `config/extract.rs` (was 8%):
  - **Single matrix test over all conflicting `--x` / `--no-x` pairs**
    (owner, apply-atime/mtime, xattrs, acls, selinux, same-permissions,
    apply-mode/owner/group/transform): each pair on the SAME command line →
    `Err(Config)`; each flag alone → `Ok`. One test, iterating the pairs table
    (not one test per pair).
  - missing / non-file archive → `Err(Config) "archive does not exist"`.
  - minimal valid → `Ok`, absolute resolved paths, work dir created.
  - `--one-top-level` bare → archive stem; explicit value → kept.
  - invalid `--mode` / `--transform` → `Err(Config)`.
  - compression suffix resolution (`--compression` + archive suffix).
  - `--clean-target` + `--no-create-dirs` → `Err`.
  - `--restore-owner` → `same_owner == true`.
- `config/archive.rs` (was 57%):
  - no `-i`/`-T` → `Err` ("at least one of...").
  - invalid `--mode` / `--transform` → `Err`.
  - `--jobs 0`, `--memlimit 0` → `Err`; happy-path field assertions.
- `config/resume.rs` (was 0%):
  - `--db` + `--work-dir` → `Err`; neither → `Err`; `--work-dir` non-dir → `Err`;
    missing db file → `Err`; happy → `ResumeOverrides` mapping
    (jobs/io_jobs/exit-after-stage).
- `config/common.rs` (was 32%): `merge_pick_clone` / `merge_pick_copy` /
  `resolve_bool_flag` — pure tests (definition absent, aligns with default,
  explicit wins).
- `config.rs` root: `resolve_cwd`, `resolve_path_to_abs_path`,
  `validate_dir`/`validate_file`, `default_archive_work_dir` /
  `default_extract_work_dir`, `archive_stem` edge cases,
  `ArchivePipelineOptions`/`ExtractPipelinePhase` `next()` chains.
- `db/state.rs` (was 47%): `save_archive_runtime_state` →
  `load_archive_runtime_state` round-trip (`Some`/`None`), plus the extract-side
  state round-trip on a real `Database`.

## D. `error.rs` (was 55%)

- Construct every `FileStatError` variant and `Error`, assert `Display` output,
  `io_path()`, and `Error::io`/`Error::copy_io` mapping `Interrupted` → `Interrupted`.

## E. End-to-end orchestrators (last units before integration)

- **`archive::run`**: temp source tree (regular files, dir, symlink, duplicate)
  → `ArchiveConfig` via `TestWorld::tuned` (`input_dirs`, `archive_path`,
  `work_dir`, `write_archive_footer: true`) → `run(config, detached)` → assert
  archive exists + `has_valid_footer`, phase `Done`, work dir cleaned. Also an
  `exit_after_stage` early-return followed by a re-run that resumes to
  completion (proves the SaveState/Resume loop).
- **`unarchive::run`**: `test_support::build_footer_archive` → `run(config,
  detached)` → assert payload extracted to root, config persisted, work dir
  cleaned (drives scan→filter→rehash→prologue→place→permissions→cleanup +
  cross-phase integrity). Same exit-after-stage + resume variant.
- Pure helpers: `enter_phase` (name/kind map, both pipelines),
  `load_extract_state` (absent → default, present → stored).
- Both `run`s call `register_mpb` (global). If they flake under the parallel
  test harness, mark them to run independently or `--test-threads=1` for the
  affected names.

## Files touched

- `crates/tar-dedup/src/compression/xz.rs`
- `crates/tar-dedup/src/common/cleanup.rs`
- `crates/tar-dedup/src/config.rs`, `config/archive.rs`, `config/extract.rs`,
  `config/resume.rs`, `config/common.rs`
- `crates/tar-dedup/src/db/state.rs`
- `crates/tar-dedup/src/error.rs`
- `crates/tar-dedup/src/archive.rs`, `crates/tar-dedup/src/unarchive.rs`
- `plans/056-coverage-closing.md`

## Verification

- `cargo test -p tar-dedup --lib` (default) and `--features test-all`.
- Re-run `cargo llvm-cov -p tar-dedup --features test-all --ignore-run-fail
  --summary-only`; targets: xz ≥ 90%, cleanup ~100%, resume 100%, extract/archive
  config 60–95%, error ~90%, `unarchive.rs` ≥ 80%, `archive.rs` ≥ 80%. Only
  `cmd/*` + `db/stats.rs` should stay at 0%.
## Implementation notes (2026-10-29) — DONE

All five sections implemented. Suites: default **684 passed / 1 failed**; `test-all`
**709 passed / 1 failed** (the pre-existing `db::meta::tests::clear_archive_meta_drops_only_byte_counters`
blocker only). `cargo llvm-cov -p tar-dedup --features test-all --ignore-run-fail`
→ tar-dedup **90.16% lines / 89.26% fns / 90.81% regions**.

### Production changes (small, deliberate)
- `config/process.rs`: `ProcessOptions::validate` now also rejects `--jobs 0` /
  `--io-jobs 0` ("must be greater than 0"). Without it rayon's
  `ThreadPoolBuilder::num_threads(0)` would panic at pool build time; this is the
  guard the plan's `--jobs 0 → Err` case needed.
- `test_support.rs` fixture `write_catalog` conforms to the A3 extract invariant
  and to real manifests:
  - duplicate now carries a sha1 (A3: `sha1 IS NULL ⟹ canonical_id IS NULL`);
  - a synthetic `/` `--input-dir` source + `ref` rows (relative placement needs a
    source root);
  - both members share `(dev=1, inode=10)` (the prologue hardlink-canonical
    election groups out_tree rows by canonical `dev,inode`).
- `build_multi_catalog` duplicate also gets a digest (same A3 reason).

### Orchestrator tests findings
- `archive::run` end-to-end works out of the box (temp source tree + tuned
  `test_archive_config`, footer on). Resume flow exercised via
  `--exit-after-stage inventory` then `StartPolicy::Resume`.
- `unarchive::run` end-to-end needed three fixture-related preconditions:
  1. `build_footer_archive` fixture dedup rows must satisfy A3 (see above);
  2. the extraction **root must pre-exist** — `build_path` short-circuits when a
     directory row equals the extraction root;
  3. don't trust a *kept* DB after `cleanup_workdir`: it renames the main file while
     connections stay open, orphaning the WAL — the kept copy is pre-WAL data.
- Std `Write::write_all` retries on `ErrorKind::Interrupted`, so the
  `InterruptibleXzEncoder` force-abort test observes a single `Write::write` call
  (the mechanism itself is correct; `write_all` would spin).

### Coverage-to-target table (lines %)
xz 92.09 · cleanup 93.94 · config 91.64 · archive cfg 80.02 · common 100 ·
extract cfg 78.39 · resume 98.64 · db/state 93.91 · error 92.78 ·
unarchive.rs 89.44 · archive.rs 77.61.
Only `cmd/*` and `db/stats.rs` remain at 0% (deferred to integration testing).
