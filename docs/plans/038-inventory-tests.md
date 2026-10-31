# Plan: Inventory phase test battery (archive/inventory.rs)

Status: **agreed (2026-10-14); implementation round.**

Scope: unit tests only, inline `#[cfg(test)] mod tests` in `archive/inventory.rs`
(module-private access to `run`, `handle_dir`, `handle_from_files_line`,
`handle_entry_base`, `handle_entry`, `files_from_records`, `files_from_reader`,
`os_str_from_bytes`, `strip_transpose`, `inner_file_type`, `ERROR_PHASE`).
Reuses `crate::test_support::TestWorld` / `insert`. One production fix approved:
`purge_entries` runs `DELETE * FROM files` (invalid SQLite) → `DELETE FROM files`.

Decisions locked with the user:
- Fix `purge_entries` so the restart-purge test exercises the real purge path.
- stdin covered by unit-testing `files_from_reader` with a `Cursor` (and a failing
  `BufRead`) + `handle_from_files_line` direct tests; the `-` dispatch branch in
  `run()` stays uncovered (in-process stdin cannot be redirected).
- Overlap smoke test is in-process clap: `Cli::try_parse_from` → `ArchiveConfig::build`.
- Deliver as plan file → then the whole battery in one pass, `cargo test -p tar-dedup --lib`.

## Harness additions (test module)

- `fn test_world(tune)` → `TestWorld::tuned(...)` with `inputs.input_dirs` /
  `files_from` / `indexing` / `paths.directory` populated from real tempdir paths.
- `fn sub_bar(world) -> ProgressBar` via `world.progress.push_sub_bar("t", BarKind::Counter)`.
- `fn non_utf8_os(suffix) -> OsString` (unix `OsString::from_vec`).
- `fn write_list(world, lines) -> PathBuf` — a `-T` file (newline or `\0` per `--null`).
- `fn ref_count(world, file_id) -> u64` — raw `SELECT COUNT(*) FROM ref WHERE file_id = :id`.
- `fn source_count(world) -> u64` / `list_sources`.
- Input-delivery param: tests take `enum Input { Dir(PathBuf), List(PathBuf) }` where the
  same scan expectations run for `--input-dir` and `-T` deliveries.

## Test matrix

### Restart purge
`restart_purges_previous_result` — seed a stale row (abs path outside the tree) +
a real `tree/` input; run inventory (purge fires, `count_entries()>0`), run again;
assert stale row gone, tree rows present, `count_entries == walked`.

### input_dir scans + overlap
- `input_scan_recursive` / `input_scan_non_recursive` — tree `{a.txt, sub/{b.txt,
  c.txt}}`: recursive → 5 rows; `--no-recursion` → 3 (root + immediate children).
- `input_scan_nested_non_recursive` — `[tree, tree/sub]` with `--no-recursion` → 5
  unique rows, `sub`/`b`/`c` carry 2 refs each (preflight `file_id_by_abs_path`
  dedup, no restat).
- Overlap, both `--input-dir` and (below) cross-source:
  - `overlap_identical`, `overlap_nested`, `overlap_valid` — via
    `Cli::try_parse_from(["tar-dedup","archive","-f",<tmp arch>,"-i",…])` →
    `ArchiveConfig::build`:
    - `/a/b && /a/b` → Err (with and without `--no-recursion`)
    - `/a/b/c && /a/b` → Err recursive, Ok with `--no-recursion`
    - `/a/b && /a/c`, `/a/b/c && /a/c` → Ok
    - each asserted in both argument orders (commutative).

### files_from (+ stdin reader)
- `files_from_run` — real `-T` file (absolute + relative lines, blank lines,
  trailing separator) → entries inventoried.
- `files_from_null` — `\0`-separated `-T`, `--null` on.
- `files_from_reader_*` — `Cursor` over newline/null lists: CR stripping,
  blank/trailing filtering; a failing `BufRead` yields `Some(Err(..))`.
- `files_from_reader_skips_empty_and_cr` — blank lines and `\r\n` dropped.

### both (cross-source overlap; recursion)
- `overlap_cross_source_both_orders` — dir `/a/b` via `--input-dir` + `/a/b/c` via
  `-T` → runtime `find_overlapping_source` Err (strict) / warn+continue with
  `--no-strict-separation`; and the mirrored order (input-dir `/a/b/c`, list `/a/b`).
- Commutativity at the runtime layer: seed a dir source, then a from-file line that
  overlaps it (either nesting direction) → Err.

### handle_from_files_line
- `from_line_joins_relative_to_directory` — relative line + `paths.directory` set →
  abs row inserted.
- `from_line_overlap_errors_strict` / `..._no_strict_separation_warns` — directory
  line overlapping an existing dir source: Err(Config) vs Ok + warn.
- `from_line_sanitize_skip_non_utf8` — non-UTF-8 line, strict → early Ok, no row;
  `fail_fast` → Err.

### handle_dir
- recursion/no-recursion counted at run level (input_dir tests above).
- `dir_walk_error_records_and_continues` — chmod-000 subdir (skip if euid==0):
  session error recorded, other entries inventoried, Ok.
- `dir_walk_error_fail_fast_aborts` — same tree with `fail_fast` → Err(Config).
- `dir_dereference_follows_symlink_dir` / `dir_no_dereference_keeps_link` —
  `tree/link -> tree/sub`: dereference → sub contents inventoried; else link
  inventoried as a symlink row.
- `dir_one_file_system_skips_other_dev` — `tree/link -> /proc` with
  `dereference + one_file_system` → no `/proc/…` rows (probe-gated: skip if /proc
  unreadable).

### handle_entry_base
- `entry_base_preflight_adds_ref_no_restat` — pre-inserted row → early return,
  ref added, `count_entries` unchanged.
- `entry_base_sanitize_skip_non_utf8` — strict → early Ok no insert; `fail_fast` → Err.

### handle_entry
- `entry_symlink_metadata_error` — dangling path → session error recorded + `Err`.
- `entry_symlink_valid` — `link_dst` captured, `Symlink(LinkType::File)`.
- `entry_broken_symlink_unknown_enc_err` — `Symlink(LinkType::Unknown)` + one
  `errors` row for the file (enc_err ingest).
- `entry_char_block_device` — `mknod` char/block (probe-gated root/CAP_MKNOD);
  major/minor captured.
- `entry_fifo` / `entry_socket` — `mkfifo` / `UnixListener::bind`.
- `entry_metadata_gating` — xattr set on the file: `capture.do_xattrs` true → row
  `xattrs` Some, false → None (probe-gated); same shape asserted for
  `do_posix_acl` / `do_selinux` (ACL/SELinux probe-gated, VM).

### debug_assert battery (catch_unwind; low priority)
`debug_run_no_inputs`, `debug_run_input_not_dir`, `debug_entry_base_relative`,
`debug_entry_relative_san`, `debug_from_line_relative_empty_directory` — each
`catch_unwind` returns Err (a panic was caught).

## Findings / notes
- `FileType::Unknown` is unreachable on unix (`determine_file_type` covers every
  `file_type()` arm); covered instead by broken-symlink `Symlink(LinkType::Unknown)`.
- `TestWorld` canonical config sets `indexing.no_hardlink_detection = true` →
  `set_hardlink_canonicals` skipped in `run`; the dedicated test calls
  `db.set_hardlink_canonicals()` directly with real dev/inode rows (real hardlinks).
- Char/block device + xattr/ACL/SELinux + chmod-000 cases use support probes
  (skip with a printed note when unavailable/privileged).

## Build / test
- `nix develop -c cargo test -p tar-dedup --lib archive::inventory` while iterating;
  `nix develop -c cargo test -p tar-dedup --lib` for the full suite (dev profile only).
