# AGENTS.md — tar-dedup project guide

Context for AI coding agents. Read `Readme.md` for user-facing docs; this file is a fast
orientation for working in the codebase.

## What this is

A `tar` "wrapper" that **deduplicates a file tree before archiving** for efficient cold
storage of unstructured data. It supports resumable archive/extract runs, sparse-file
materialization, and per-file metadata capture (times, uid/gid, mode, xattr, ACLs,
SELinux, symlink targets).

Design stance (from `Readme.md`):
- No delete/modify/append on archives — assume cold data, "write once, extract whenever".
- Work is asymmetric: archiving is expensive (3–4+ reads/file), extraction is cheap.
- Compression defaults to xz (most aggressive). Not backwards compatible before v1.0.0.
- Stored paths are **absolute**; relative mapping happens only at extract.

## Workspace layout

Rust workspace, edition 2024, `rust-version = 1.95`. Three crates:

| Crate | Kind | Purpose |
|-------|------|---------|
| `crates/tar-dedup` | lib | Core pipeline, DB facade, tar read/write, footer, phases. |
| `crates/tar-dedup-cli` | bin (`tar-dedup`) | Thin `clap` entrypoint dispatching to `archive::run` / `unarchive::run` / resume. |
| `crates/sparse-cp` | lib + bin | Sparse-aware copy and zero-block page counting (`--sparsify` support). |

`crates/tar-dedup/src` module map (`lib.rs`):
- `archive.rs` — archive pipeline orchestrator (phase loop).
- `unarchive.rs` — extract pipeline orchestrator.
- `cli.rs` — all `clap` arg structs + `ExitAfterStageArg`, `ConflictPolicy`.
- `config.rs` — config builders (`ArchiveConfig`, `ExtractConfig`, `ResumeConfig`); submodules `config/{archive,compression,extract,paths,phases,process,resume}`.
- `db.rs` — `Database` facade over `rusqlite`, delegating to `db/{schema,types,flags,inventory,hash,filter,dedup,sparsify,stage,tar_writer,extract,place,rehash,scan,errors,integrity,common,meta,content_id,permissions,source}`.
- `archive_footer.rs` — the seekable sqlite trailer appended to finished archives.
- `common.rs` — shared constants (`COPY_STEP_SIZE` 4 MiB, `DEFAULT_BATCH_SIZE` 100_000, manifest/snapshot tar names).
- `tar_reader.rs`, `tar_builder` — tar stream plumbing.
- `compression.rs`, `progress.rs`, `shutdown.rs`, `error.rs`.

## Two pipelines, one SQLite state machine

Both use a **work-directory SQLite DB** as the resumable state. Work dirs default to
`{stem}.astage` (archive) and `{stem}.estage` (extract) beside the archive. A run resumes
by reloading the phase from `meta` and reprocessing the same DB. `--exit-after-stage`
needs a phase; `resume` only allows overriding `--jobs` / `--exit-after-stage`.

### Archive pipeline (`archive.rs` → `archive/` submodules)
`inventory → hash → filter → dedup → sparsify → stage → archive(tar)`

1. `inventory.rs` — walk `-i` / `-T` sources, record `source` + `files` rows with metadata (absolute paths).
2. `hash.rs` — content SHA-1 (+ zero-page scan) via rayon; updates digests in `files`.
3. `filter.rs` — apply include/exclude regexes; `ingest_filters` populates `filter_reason`; `apply_no_filter` catch-all.
4. `dedup.rs` — group by `(sha1, size)`; elect canonical (self-link); dupes point `canonical_id` at it.
5. `sparsify.rs` — `sparse-cp` materializes hole-y files into the work dir when `--sparsify`.
6. `stage.rs` — symlink canonical work-dir payloads under the stage root.
7. `tar_builder` — write canonical files + `manifest.sqlite` / `snapshot.sqlite` into a (compressed) tar stream; finalize, then `archive_footer::write_footer`.

Each phase drives `files.phase` forward (`inventoried → hashed → filtered → deduped →
sparsified → staged → archived`), tracked in `FilePhase` (`db/types.rs`).

### Extract pipeline (`unarchive.rs` → `unarchive/` submodules)
`scan → rehash → place → permissions → cleanup`

1. `scan.rs` — `archive_footer::read_footer` installs the manifest DB; scan tar payloads into extract cache.
2. `rehash.rs` — optional content verification.
3. `place.rs` — build `out_tree` (target paths), `populate_out_tree` materializes each path (copy, hard-link, or symlink per `--link-tree`).
4. `permissions.rs` — restore metadata last.
5. `cleanup` — remove work dir unless `--keep-stage` / `--keep-db`.

## SQLite schema (`db/schema.rs`)

- `meta(key, value)` — runtime state, archive meta, byte counters.
- `filter_reason(id, source, line, expression)` — id `> 0` = exclude, `< 0` = include, `0` = internal catch-all.
- `source(id, source, abs_path, original_path, line, flags)` — declared inputs.
- `files(id, abs_path UNIQUE, ext, size, sha1 BLOB, mtime/atime/ctime, uid/gid, username/groupname, mode, ftype, inode, dev, major, minor, new_name, xattr, acl, selinux, link_dst, sparse_count, include_reason/exclude_reason, canonical_id, phase, flags)` — one row per filesystem entry; `canonical_id = id` marks canonical payloads.
- `ref(source_id, file_id)` — source membership.
- `out_tree(id, canonical_id, abs_path UNIQUE, file_id, flags)` — extraction destination tree.
- `ref_out(out_id, source_id)`.
- `archive_sessions(id, archive_offset, finalized, started_at, finished_at)` — tar stream sessions.
- `errors(id, file_id, out_tree_id, abs_path, error_msg, error_type, phase, error_misc, error_datetime, flags)` — persistent error log (see `db/errors.rs`); `flags` holds an `ErrorFlags` bitset (`ErrorFlag::SessionError` marks config/session scoped rows).

Indexes: `files(sha1, size)`, `files(canonical_id)`, `files(phase)`, `files(abs_path)`, `out_tree(file_id)`, `out_tree(abs_path)`.

DB access pattern: `Database` in `db.rs` is the only public API for the pipeline; each method
delegates to a per-area module function taking a raw `&Connection`. Batch reads return
`Vec<R>` where `R: SqlFileRow` (thin typed row structs). `Database` wraps `conn` in
`RefCell<Connection>` (single-threaded; rayon workers do I/O off-DB). Schema uses WAL,
`synchronous=NORMAL`.

## Archive container format

```
[ compressed(tar archive) | MAGIC | xz(-9e) sqlite | sha1(xz blob) | MAGIC | u64 offset ]
```
- `MAGIC` = `b"Tar-Dedup-SQLite-Footer"` across both sides.
- Footer payload is the finalized work DB (compressed with fixed `xz -9e`, independent of the stream compression).
- `u64 offset` (little-endian) points at the first `MAGIC`; used to locate the trailer via `SeekFrom::End`.
- Integrity: sha1 over the `xz` blob; corrupted/missing footer fails validation (`has_valid_footer`).
- First tar member must be `manifest.sqlite` (the initial DB); later `snapshot.sqlite` DBs mark progress checkpoints. See `archive_footer.rs` and `common.rs` names.

### Canonical payload naming
`{hash_b64}.{fsize_b64}.{fid_b64}.ext` — base64url (url-safe, no padding) in `db/content_id.rs`.
One physical copy per `(sha1, size)` cluster; metadata lives in `files` rows.

## Conventions (also in `.cursor/rules/rust-style.mdc`)

- **DO NOT reformat existing code.** This repo deliberately deviates from rustfmt/the usual
  style. The author optimizes for density + readability balance. In particular:
  - Function arguments may be grouped semantically: two lines of args ≠ one arg per line.
    A two-line split groups related params; respect the existing grouping, never explode a
    signature to one-arg-per-line.
  - Do not run `cargo fmt` on existing files; when editing, mimic the surrounding style
    (tabs/spaces, wrapping, blank-line rhythm) — do not reformat the rest of the file.
- **One function when asked for one** — do not split into helpers/wrappers unless asked.
- **`expect` for contract violations, `Result` for user/input faults.** Schema init, prior-stage invariants, FK rows inserted by the caller → `expect("…")`. Do not sprinkle `ok_or_else(|| Error::Config(...))` in leaf functions.
- **Read the call site before editing pipeline code.** Leaf pipeline functions assume preconditions established upstream; don't "fix" them by re-inserting rows or weakening invariants.
- Errors: `crate::error::Error` (config vs io vs interrupted). `Error::Interrupted` is handled by the phase loop to save state and exit cleanly.
- **Persistent error log** (`errors` table, `db/errors.rs`): every non-halting filesystem error should become a `FileStatError` (add variants like `Nix`/`General` as needed), be pushed into a `Recorder` (`db::Recorder::new(db, enabled)`), and flushed in a single transaction at a batch/phase boundary (or on `Drop`). `--no-errors` (`config.process.no_errors`) disables recording. Do not swallow an FS error down to a bare `Error`/flag without persisting it. `ErrorScope` is an OR-able bitset over `File`/`OutTree`/`Session` for querying.
- No code comments unless they explain tricky invariants; match existing style.
- `FileType`/`LinkType` round-trip through `as_str()`/`parse()` — keep those in sync when adding variants.

## Build / test / dev

> **Tests are a work in progress — do not block on them.** The integration tests
> (`crates/tar-dedup/tests/`) were partially written by another LLM with partial
> information: they are incomplete, don't cover the whole codebase, and may rely on
> outdated schema/API assumptions. Priority is finishing the construction and design
> of the pipelines; the test suite is a follow-up task. If a test can't compile or
> fails, treat that as expected, fix the production code on its own merits, and
> defer test repair.

- Dev shell: `nix develop` (flake) provides rust toolchain + native libs (`flake.nix`); on Debian install `libselinux-dev libclang-dev clang` instead.
- Build: `cargo build` / `cargo build -p tar-dedup-cli`.
- Test: `cargo test` (unit tests inline in modules; integration tests in `crates/tar-dedup/tests/`: `archive_footer.rs`, `db_extract.rs`, `out_tree.rs`, `types.rs`, `common/`).
- No clippy/CI config in repo; no `.github` workflow.
- `sparse-cp` has its own heavy 1 GiB unit tests (large sparse-file math) — run under `-p sparse-cp` to isolate.

## Gotchas / notes

- `rust-version = 1.95` and edition 2024 — modern std APIs fine.
- `files.abs_path` is always absolute; filtering regexes match the absolute path, and a filter matching a directory excludes the directory metadata but NOT its contents (documented deviation from tar).
- `--fresh` wipes work dir; a `.lock` file (fs4) guards concurrent runs.
- Debug feature `debug-force-utf8` makes DB dumps readable but panics on non-UTF8 bytes.
- Old "raw sqlite footer" archives are not supported — only the current `MAGIC` trailer.