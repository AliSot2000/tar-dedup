# 095 - Optional stage pass; archive reads payloads from the DB

Status: **implemented (2026-11-16).** `cargo test -p tar-dedup-lib --lib`: 784 passed.

## Motivation

The archive stage pass currently walks every canonical file and creates a symlink
in `{workdir}/{content_id}` pointing at `record.fs_path()`. The tar builder then
resolves that symlink back to the same file. This is pure overhead: the stage
symlink and the DB row point at the same source path. The only payload that
genuinely needs to be staged is the sparsified copy produced by `sparsify`.

`--bypass-archive` is different: its product *is* the stage directory, so it
still needs the symlink pass (plus the sparse copies).

## Decisions

- Default: no filesystem stage pass. The `Stage` phase only performs the DB
  promotion.
- `--add-stage-pass` opts into the filesystem pass for debugging/investigation.
- `--bypass-archive` implies the filesystem pass (same job as `--add-stage-pass`).
- No `--no-stage-pass` flag.
- The DB is always the archive source of truth: non-sparse payloads come from
  `record.fs_path()`.
- No backwards-compatibility shims (< v1.0.0): no `serde(default)` for the new
  config field, no old-config fallback.

## Config / CLI

Add to `ArchivePipelineOptions`:

```rust
pub stage_pass: bool,   // default false
```

Add to `ArchiveArgs`:

```rust
/// Run the filesystem stage pass (canonical symlinks) for debugging.
/// --bypass-archive enables this automatically. Default: DB-only promotion.
#[arg(long = "add-stage-pass", default_value_t = false, action = ArgAction::SetTrue)]
pub add_stage_pass: bool,
```

Resolve with the existing helper:

```rust
stage_pass = resolve_bool_flag(false, args.add_stage_pass, args.bypass_archive)
```

No `serde(default)`: old serialized configs are not supported.

## Stage phase

### Default (`stage_pass == false`): DB-only

1. `promote_unstageable_files()`
2. bulk `UPDATE files SET phase = 'staged' WHERE phase = 'sparsified'`
3. log/progress; no `stage_dir` creation, no symlinks, no FS checks.

Add `db::stage::promote_sparsified_to_staged(conn) -> Result<u64>` for step 2.
After step 1 every remaining `sparsified` row is an eligible canonical payload.

### Filesystem pass (`stage_pass == true`)

Keep the current implementation: create symlinks for non-sparse canonicals and
verify the sparsified copies, then set `staged`.

Comment in `stage.rs`: the stage directory should contain only the copies we are
forced to make (sparsification). The symlinks exist solely for the debug/bypass
path.

## Normal tar builder (`archive/tar_builder.rs`)

Per `StrippedRecord` in the archive queue:

```text
if record.flags.HasSparse:
    sparse = stage_dir/{content_id}
    if sparse.is_file():
        payload = sparse
    else:
        warn!("sparse payload missing at {sparse}; falling back to {fs_path}")
        record_file_error(recorder, record.id, sparse, sparse-missing)
        payload = record.fs_path()
else:
    payload = record.fs_path()
```

- `record.fs_path()` (raw path when present), never `record.abs_path`.
- No `fs::canonicalize` in production payload resolution.
- If the selected payload is missing/unreadable, keep the existing per-file
  error path (`ErrorWhileArchive` + recorder) and continue to the next file.
  Honour `--fail-fast`; otherwise no hard abort.
- The sparse-missing warning is also recorded with the existing `Recorder`
  (`record_file` with the Archive phase), file-scoped to `record.id`. It does
  **not** set `ErrorWhileArchive`, because the fallback still archives the
  source successfully.

Timestamp warnings:

- `warn_if_times_changed(record.fs_path(), ...)` only.
- Only when `record.fs_path()` exists.
- Never on a `stage_dir/{content_id}` path: sparsified copies have different
  timestamps, so checking them is meaningless.

Debug-only cross-check in `tar_builder.rs` (not `stage.rs`):

```rust
if stage_pass && !record.flags.HasSparse {
    let link = stage_dir.join(&tar_name);
    if link.exists() {
        debug_assert_eq!(
            fs::canonicalize(&link).expect("stage symlink resolves"),
            fs::canonicalize(record.fs_path()).expect("source resolves"),
            "stage symlink must resolve to the DB-recorded source",
        );
    }
}
```

This checks the stage output against the DB at the consumer; it is never used
for resolution and disappears in release builds.

## Bypass builder

`run_bypass_archive` is separate and keeps resolving its stage entries: the
product directory is the stage directory.

- Keep the product-integrity error handling.
- Fix the timestamp check to use `record.fs_path()` (the original), not the
  `content_id` target. Skip the check when the original is gone.
- Add a comment: the product intentionally contains symlinks plus sparse copies,
  not full canonical copies. `--sparsify --min-pages 0` materialises full
  payload copies; the `--bypass-archive` CLI help already documents this.

## Tests

- `db/stage`: bulk `sparsified -> staged` promotion.
- `archive/stage`: existing FS tests set `stage_pass = true`; new default test
  asserts no symlinks and all eligible rows `staged`.
- Normal `tar_builder`:
  - non-sparse reads `fs_path()`;
  - a stale/broken stage symlink is ignored;
  - missing sparse warns, records a file-scoped error, and falls back to source;
  - missing source records the existing per-file error and continues;
  - timestamp flags are computed from `fs_path()`, not the sparse copy.
- Bypass: broken payload still errors/records; timestamps use the original.
- End-to-end archive in default and `--add-stage-pass` modes.
- Bypass round-trip unchanged.
- Config: default false, `--add-stage-pass` true, `--bypass-archive` true.

## Docs

- This plan file.
- `AGENTS.md`: `Stage` is DB-only promotion by default; the filesystem pass runs
  only with `--add-stage-pass` or `--bypass-archive`.
