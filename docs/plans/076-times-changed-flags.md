# 076 — Split `Modified` into per-timestamp change flags

Status: **agreed (2026-11)**, not yet implemented (context will be compacted
after this file is written; resume here).

## Problem

`warn_if_times_changed` folds all three timestamps into one `FileFlag::Modified`.
This loses resolution (we can't tell *which* stamp changed) and warns noisily for
self-inflicted/non-authoritative changes (`atime` bump from our own reads, `ctime`
bump from metadata ops). We want full per-channel resolution in the DB plus a
quiet stdout.

## Design decisions (from conversation)

- **A — carrier shape:** a name-addressable struct `TimesChanged { mtime, atime,
  ctime: bool }` carried through worker→ingest (not a raw bit accessed as `flag.0`).
- **B — ingest write:** a new `add_file_flags(conn, id, FileFlags)` union
  (`flags = flags | :bits`); the existing `set_file_flags` only *replaces* the
  column, so is unusable for adding bits.
- **C — stat failure:** `warn!` and return all three `true` (fail-safe > other/no).
- **D — per-channel + logging:**
  - Each stamp compared independently at second resolution; **no** propagation
    from mtime to atime/ctime — set exactly the bits read.
  - Log `changed` = comma-joined set of changed stamps **in order `atime,ctime,mtime`**.
  - Level: `warn!` iff `mtime` changed; else `debug!` iff any changed; else silent.

## Changes

**1. `db/flags.rs`**
- Replace `FileFlag::Modified` with `MtimeChanged`, `AtimeChanged`, `CtimeChanged`;
  renumber the enum densely (pre-1.0.0 / `--fresh`; no display mapping exists).
- Add `add_file_flags` union helper (facade in `db.rs`).
- Fix the flags round-trip test if it references `Modified`.

**2. `common/files.rs`**
- Add `TimesChanged { mtime, atime, ctime: bool }` (Copy, Send, Default=none).
- `warn_if_times_changed` → returns `TimesChanged`; per-stamp, second-resolution;
  stat-failure → warn + all-true; logging per D.
- `impl TimesChanged { fn to_file_flags(&self) -> FileFlags }`; a
  `changed_names()`-style helper for the `atime,ctime,mtime` message.

**3. Carriers hold `TimesChanged` (replace `modified: bool` / `(id, bool)` tuples):**
- `db/hash.rs`: `HashSuccess.times`, `HashError.times`; ingest :254/:262 →
  `add_file_flags(id, times.to_file_flags())`.
- `db/sparsify.rs`: `SparseOutcome.times`; ingest :202.
- `db/dedup.rs`: `canonical_times` / `candidate_times`; ingest :792/:795.
- `archive/stage.rs`: result tuple `(id, times)`; ingest :119/:124.
- `archive/tar_builder.rs:159`: `add_file_flags(id, times.to_file_flags())` on append success.
- Workers: `let t = warn_if_times_changed(path, m, a, c);` → `times: t`; guard a
  no-op `add_file_flags` when the flags are empty.

**4. Tests**
- Update ≈30 `modified: true/false` constructors + `get(FileFlag::Modified)`
  assertions → `TimesChanged`/per-bit equivalents (hash, sparsify, dedup, stage,
  tar_builder).
- New unit tests: mtime-bump → only `MtimeChanged`; atime-only → only
  `AtimeChanged`; both → both bits (no crosstalk); logging string order
  `atime,ctime,mtime`.

## Files touched
`db/flags.rs`, `db.rs`, `common/files.rs`, `db/hash.rs`, `db/sparsify.rs`,
`db/dedup.rs`, `archive/stage.rs`, `archive/tar_builder.rs`, `archive/hash.rs`,
`archive/sparsify.rs`, `archive/dedup.rs` (worker call sites) + the module tests.

## Verification
Local `cargo test -p tar-dedup-lib --lib`, then VM `--features test-all`
(`test-compression` compile is separate from default lib tests).