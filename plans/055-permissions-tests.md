# Plan: tests for `unarchive/permissions.rs` (+ callees)

Status: **agreed (2026-10-23).** Unit battery for the permissions (metadata
restore) phase and its pure-callee gaps. Only `unarchive/permissions.rs` gets a
test module; do **not** touch the `unarchive.rs` orchestrator. Full
archive→extract round-trip deferred to the integration-testing phase.

## Decisions (final)

- **Round-trip integration test**: deferred (integration phase).
- **"fail fast"** = the `rt.config.process.fail_fast` branches in
  `inner_apply_permissions` and `apply_permissions_link_sources` only.
- **Capability probes reused**: `probe_xattr` / `probe_acl` / `probe_selinux`
  become `#[cfg(test)] pub(crate)` in `common/xattr.rs`; permissions gate-tests
  early-return when the host/FS lacks support.
- **`parse_mode_changes`**: the octal `set_str("={digits}")` arm is provably
  infallible (post `from_str_radix` + `≤0o7777` → pure `0-7`) → convert to
  `expect("PRECONDITION FAILED: …")`. The symbolic arm stays `map_err` — it is
  still reachable (`"ugo"`/`"a"` = users without operator, `"u+w,"`/`","` =
  empty comma-clause), and it is a user-input fuse (`Result`, not `expect`).
  **No custom pre-parser** (avoid re-implementing `file-mode` grammar).
- **Fixtures**: realistic valid file types (`.docx .pptx .xlsx .pdf .txt .html
  .css .typst .json .xml .yaml .jpg .jpeg .heif .heic .gif .arw .cr2 .dng
  .mp4`) with valid UTF-8 content (structured vs freeform); extensions flow
  through `content_id` into `.sources` link names.

## Test infrastructure

`PermissionsWorld` = local `impl ExtractWorld` (shared `test_support` world,
`for_place_test` base + `work_dir = dp/estage`, `io_jobs = jobs = 1`, per-test
`tune`).

Seeders:
- `write_file(rel, payload)` → real on-disk file under the extraction root.
- `add_file(rel, payload, tune: &mut NewFileRecord)` → `files` row
  (self-canonical, `rehashed`) + `seed_filter_reasons`; `set_names(id, user,
  group)` for the `username`/`groupname` columns (absent from `NewFileRecord`).
- `add_out(rel, file_id, flags)` → `out_tree` row, sets `canonical_id = id`.
- flag helpers `out_flag` / `file_flag` / `file_phase`.
- `mk_file(...)` (pure `FileRecord`), `mk_ogp(...)` (`OwnerGroupMode`), meta
  setters via `db.set_archive_owner_policy` / `set_archive_mode_changes`.

## A. `common/perms.rs`

- `parse_mode_changes`: octal `set_str` → `expect`; new tests: foreign char
  (`"v"`, `"u+vv"`), `!octal && !symbolic` (`"not-a-mode"`), **symbolic-arm
  errors** (`"ugo"`, `"a"`, `"u+w,"`, `","`), octal boundaries (`"09"` → radix
  Err, `"7777"` ok, `"77777"` → range Err, `"0"` ok).
- `resolve_identity` / `resolve_owner_group` **full matrix**: target ∈ {Ids,
  Names, NameId, IdName} × `same_owner` {T,F} × six stages (`map.name →
  override.name → db.name → map.id → override.id → db.uid`); degrade chains and
  jumps (only `override.id` present; `map.name` present-but-unresolvable falls
  all the way to `db.uid` when intermediates are absent; single-chain targets
  never cross-dig). Uses `current_user`/`current_group`, `nonexistent_user_name`
  (and the currently-unused `nonexistent_group_name`). Helper `resolve_row`.

## B. `unarchive/permissions.rs` — new `// TEST` module

- `resolve_owner_group_policy`: 4 arms (`None`/`Cli`/`Stored`±meta) + partial
  application matrix (`apply_owner × apply_group` → which of the 4 policy
  fields transfer).
- `resolve_mode`: 4 arms; `Cli` invalid → `expect` panic (`catch_unwind`).
- `apply_one`:
  - `ogp` Some/None; uid/gid both/owner/group (gid→own gid is non-root-safe and
    asserted; owner-change asserted only as root); chown error (missing target;
    non-root `uid=Some(0)` → EPERM, gated).
  - `no_same_permissions` gate; symlink gate (mode skipped, lchown still runs);
    `record.mode=None`; mode-apply error (missing target).
  - xattr/acl/selinux `no_*` gates (skip-early on capability probe failure).
  - times: `apply_times` direct 4 combos incl. `(None,None) → Ok`; gate
    precedence matrix `apply_mtime × apply_atime × has-mtime × has-atime`.
  - exact error counts; a cascade (missing target across the setters) and a
    "fails only at step 2" case (chown ok, mode fails).
- `inner_apply_permissions`: record `None` (ancestor dir) → `(id,true)`, no
  capture; fail-fast → `Err(Error::Other)`; non-fail-fast → `capture_err` per
  error + `(id,false)`; `inc_both(1)`.
- `apply_permissions_link_sources`: happy (+ custom `link_source`); the
  `content_id` `expect` panic under `catch_unwind`; fail-fast → `Err(Other)`;
  error via `rec.record_file`.
- `process_batched_files` / `process_batches_dirs`: rows → out-tree
  `AppliedMetadata` / `ErrorWhileApplyingMetadata`; graceful → `Err(Interrupted)`
  with no ingestion.
- `run`: `link_tree` F × `no_overwrite_dir` F (non-dir + dirs applied); 
  `no_overwrite_dir` T (dirs skipped); `link_tree` T (`.sources` applied +
  `file AppliedMetadata`); **`PermissionsRestored` on ALL `files` rows** (incl.
  one without an `out_tree` entry); out→file flag propagation
  (`apply_permissions_flags_to_files`).

## Verification

- `cargo test -p tar-dedup --lib -- unarchive::permissions`, then
  `common::perms`, `common::xattr`, then full default + `--features test-all`.
  Expect only the pre-existing blocker
  (`db::meta::tests::clear_archive_meta_drops_only_byte_counters`).

## Notes for the implementation session

- `permissions.rs` has no test module yet; the `// TEST` banner goes at the end
  (dashes to column 120).
- `OwnerGroupMode { ogp, mp }` fields private — construct directly (same module).
- `apply_one(config, &FileRecord, tgt: &Path, ps) -> Vec<FileStatError>`.
- Time gate: `(apply_mtime && mtime.is_some()) || (apply_atime && atime.is_some())`,
  each side independently `None`-ed. `debug_assert!(errors.len() <= 6)`.
- DB: `set_archive_owner_policy` / `set_archive_mode_changes`; listers/counters
  in `db/permissions.rs` via the `Database` facade; flags `FileFlag::AppliedMetadata` /
  `ErrorWhileApplyingMetadata`, `OutTreeFlag::AppliedMetadata` /
  `ErrorWhileApplyingMetadata`.
- Shutdown: `request_graceful` → `check_between_files` errors.