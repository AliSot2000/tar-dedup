# 082 — Advanced compression (JSON) + `--bypass-archive`

Status: **implemented and verified.**

## Scope

Two connected features:
1. **Advanced compression via JSON blobs** (`--xz-advanced`, `--zstd-advanced`) as the power-user surface over the base `--jobs`/`--level` knobs.
2. **`--bypass-archive`**: produce a *directory* product (canonical payloads + a finalized `tar-dedup.sqlite`) instead of a tar archive, so another tool compresses/stores it (tar-dedup out of the loop); and, on extract, *validate* such a pre-extracted product dir instead of untarring.

## A. Compression interface

**Top-level knobs** remain `--jobs` and `--level`. **All `--xz-*` flat flags are dropped** (`--xz-dict-size`, `--xz-memlimit-compress`, `--xz-extreme`, …).

**Value syntax** for both advanced flags: an inline JSON string, **or `@/path/to/blob`** to read the JSON from a file (valid JSON never starts with `@`, so `@` unambiguously selects file mode). Documented in each flag's help.

**`--xz-advanced <JSON>`** — all keys optional; overlay on top of the `--level` preset; JSON value wins when present:
```
level, extreme, dict, block_size, block_list, threads,
lc, lp, pb, mode(fast|normal), nice, mf(hc3|hc4|bt2|bt3|bt4),
depth, check(none|crc32|crc64|sha256), memlimit
```
(No BCJ/delta filters — niche ISAs; escape hatch is the bypass product + another tool.)

**`--zstd-advanced <JSON>`**:
```
level, threads, long_window_log, checksum(bool), content_size(bool), dict(path)
```

**Format policy**: gzip stays level-only; **bzip2 kept as a format, no knobs, no JSON** (compat only). No JSON blob for gzip/bzip2.

### Encoder plumbing
- **xz** (`compression/*`, `tar_writer.rs`): when only preset-style knobs are present → `MtStreamBuilder` as today (`preset`, `threads`, `block_size`, `check`, timeout). When filter knobs are present (`dict`, `lc/lp/pb`, `mode`, `nice`, `mf`, `depth`) → build `Filters` with a `LzmaOptions` (from `new_preset(level[extreme])` + overrides) and pass via **`MtStreamBuilder.filters()`** → **custom dict × `--jobs` threads works** (fixes the earlier single-thread limitation).
- **zstd** (`tar_writer.rs`): `zstd::stream::Encoder` with `multithread(threads)`, `long_distance_matching(long_window_log)`, `include_checksum`, `content_size`, and a trained dictionary loaded from `dict(path)`.
- The advanced structs are `Serialize`/`Deserialize` and stored on `CompressionSettings`, so they round-trip into the `archive_config` meta.

### Config/CLI work
- `cli.rs` `ArchiveArgs`: add `--xz-advanced`/`--zstd-advanced` (`Option<String>`), remove the `--xz-*` flags; keep `--level`, `--jobs`.
- `config/compression.rs`: add `XzAdvancedOptions`/`ZstdAdvancedOptions` (serde, all-`Option`), parse `@file`/inline, validate (dict ≤ LZMA2 max, only-xz, etc.), fold into `CompressionSettings`.
- `config/archive.rs`: merge by `merge_pick_clone`; carry through.

## B. `keep_stage` tri-state

Replace `CleanupSettings.keep_stage: bool` with:
```rust
enum StageKeep { Never, OnOther, Always }
```
- default **`OnOther`** (keep stage only when unknown files are present — used by bypass extract); `--keep-stage`→`Always`, `--no-keep-stage`→`Never`.
- Update `config/process.rs`, `config/cli` flags, `unarchive/place.rs` `cleanup_stage` logic, and the `resolve_bool_flag` call sites.

## C. `--no-rehash`

Hard **error** with hint `"use --force"` (bypass products have no stream sha1, so content rehash is the integrity guarantee; `--force` is the explicit "data may be invalid" escape). Applies on extract.

## D. `--bypass-archive`

### Mode validation (hard error, at CLI/config build)
- `-f <dir>` without `--bypass-archive` → error (`-f` is a directory; pass `--bypass-archive` or a file).
- `--bypass-archive` with `-f <file>` → error.
- No silent inference. Emit `tracing::info!` once the resolved mode is set. The resolved `bypass` flag is the only source of truth; **`scan.rs` does not recheck dir-vs-file**.

### Meta
New `bypass_archive` key in `db/meta.rs` (`MetaKey::BypassArchive`, `bypass_archive`).

### Archive side
Run the normal pipeline to `stage`, then the **real archive finalize** (reuse machinery so integrity flags land identically):
1. create an archive `session`,
2. `pull_pending_archives` (same selection criteria as the tar path),
3. `promote_*` rows → `archived`, set `FileArchived` flags,
4. `finalize_tar_writer_session`,
5. snapshot/finalize the work DB and write it as **`tar-dedup.sqlite`** into the product dir, next to the canonical payloads.
No tar, no footer, no sha1. `bypass_archive = true` in meta.

### Stage/sparsify refactor (real-vs-symlink payloads)
- **sparsify**: materialize the sparse file **directly at the canonical `content_id`** (not a temp that stage later symlinks to).
- **stage**: if the row has `HasSparse` ⇒ just mark `staged` (the file is already in place); otherwise create the `content_id` **symlink** (as today).
- Documented in CLI/manual: *"for an all-copied product dir use `--sparsify --min-pages 0`."*

### Extract side (`-f <dir>`)
1. read `tar-dedup.sqlite`, confirm `bypass_archive` meta,
2. **index the directory** in a `check_in_flight` loop; **abort ⇒ roll back the DB** (nothing committed),
3. per entry:
   - `tar-dedup.sqlite` → **passed** (skip),
   - a `content_id`-named file → **probe-read `io_buffer` (4 MiB)**: ok ⇒ set `FileExtracted`; read error ⇒ set `ErrorWhileExtracting` + error log,
   - an unknown / non-`content_id` file → `--fail-fast` ? **abort** : **warn** "unexpected file" + `StageKeep::OnOther` keeps stage,
4. **reuse the scan-style status summary** (N archived / N missing / N errored) so integrity accounting matches the tar path,
5. then normal `place → permissions`.

## E. Tests / verification

- Advanced-JSON parse + overlay onto preset (unit),
- `@file` parsing,
- xz dict×threads round-trip (via `filters`),
- zstd advanced (threads, long, checksum, content-size, dict) round-trip,
- `keep_stage` tri-state behavior,
- `--no-rehash` → error with `--force` hint,
- `--bypass-archive` mode-validation hard errors (dir-no-flag / flag-with-file),
- bypass archive → product dir (`tar-dedup.sqlite` + canonicals) → bypass extract round-trip,
- bypass extract scan-status summary + unknown-file warn/fail-fast + abort-rollback,
- a few `integration/` scenarios.

## Files touched (main)

- `cli.rs` (advanced flags; drop `--xz-*`; `StageKeep` flags; `--no-rehash` error; `--bypass-archive`)
- `config/compression.rs` (`XzAdvancedOptions`/`ZstdAdvancedOptions`, parse, validate)
- `config/process.rs` (`StageKeep` replacing `keep_stage`)
- `config/archive.rs` (merge/try_from carry-through)
- `compression/xz.rs` (`MtStreamBuilder.filters(LzmaOptions)`)
- `compression/*.rs`/`tar_writer.rs` (zstd advanced + xz filter plumbing)
- `db/meta.rs` (`BypassArchive`)
- `db/tar_writer.rs` (reuse finalize machinery for bypass)
- `archive.rs`, `archive/tar_builder.rs` (bypass finalize path)
- `archive/sparsify.rs`, `archive/stage.rs` (materialize at canonical_id)
- `unarchive.rs`, `unarchive/scan.rs` (bypass extract path, mode from flag, status summary)
- `db/scan.rs` (bypass index/validate helpers if needed)

## Implementation order

1. Compression JSON (config + CLI + encoders + tests).
2. `keep_stage` tri-state + `--no-rehash` error.
3. `--bypass-archive` (meta, mode validation, archive finalize, sparsify/stage, extract path).
4. Integration scenarios + VM `--features test-all`.