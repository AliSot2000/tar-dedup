# Plan: Filter test battery

Status: **implemented (2026-10-17), 22 tests + 1 production fix.** Covers
`common/filter.rs` (bulk), plus the `archive/filter.rs` and `unarchive/filter.rs`
wrappers. Uses `TestWorld` / existing helpers only (no new `test_support.rs`
support).

## 1. `handle_query` (common, private fn)

Table-driven with a collecting sink `(from, line, query)`:

- **Ok path**: valid regex → sink receives `(source, Some(line), query)`.
- **Err path**: invalid regex (e.g. `[`) → `Ok(())`, nothing inserted (recording
  not asserted; recorder runs disabled).
- **ignore_case**: `"keep"` matches `KEEP.txt` in `test_match` with `ignore_case`.
- **Flag plumbing probe**: `\p{Greek}` is a *unicode-only* construct — compiles and
  inserts under unicode mode, is rejected (not inserted) under `--no-unicode`.
  (Deviation from the original plan: `\xFF` was assumed unicode-rejected, but the
  regex crate compiles it in both modes, so it cannot probe the flag; `\p{Greek}`
  replaced it.)

## 2. `ingest_filters` matrix

- **Matrix sweep** `(cli, lines, files)` ∈ {0,1,5}×{0,1,5}×{0,1,2,3}: total
  inserted == `cli + files*lines`; from-file lines carry 0-based line indexes.
- **Joint single call**: 1 CLI + populated file + empty file + broken file, for
  both include and exclude arms.
- **Internal catch-all** appended (from `"internal"`, `None`, `".*"`) iff no user
  include landed (`user == 0`).
- **Broken file** `fail_fast=true` → `Err(Config)`.

## 3. `test_match` matrix

`Vec<MatchCase>` (list of checks) crossed with two path variants (UTF-8 `café`,
non-UTF-8 raw bytes):

- 4 base cases → `(0,0)` / `(<0,0)` / `(0,>0)` / `(<0,>0)`.
- **first-match-wins**: two matching includes → the *first* id wins; same for
  excludes. (Existing raw-bytes + utf8 tests kept.)

## 4. `parse_filter`

- Anchored: `foo` → `^foo`; `^foo` never double-anchored; unanchored untouched.
- `ignore_case` + `--no-unicode` compile flags behave (`\p{Greek}` rejected under
  no-unicode; `\xFF` matches the raw byte).
- **Panic** via `catch_unwind` for an invalid expression (`[`).

## 5. `step_filters` / `joint_filter_phase` (DB-backed)

Real archive closures (mirror `archive::filter::run`).

- **Early-out**: seeded internal `.*` (1 include, 0 excludes) → `apply_no_filter`
  fast path; every row `(-1, 0)` + `filtered`.
- **Count accuracy via DB outcome**: `clear_internal_include` first (TestWorld
  unconditionally seeds the internal `.*`, which production only installs when
  there are no user includes — with it present `include_reason` can never be 0),
  then user patterns → cells `(-1,0) / (-1,1) / (0,1) / (0,0)` =
  included / excl-by-exclude / excl-by-include / excl-by-include.

### Assert tests (compact, `catch_unwind`)

- **A** (`include_reason <= 0` guard): mocked positive-id include (id 5) via the
  `get_filters` closure + `raw_record` → panics.
- **B** (leftover rows): real DB, one `hashed` row, no-op `apply_results`
  returning the batch count → the in-loop count assert passes, the zero-leftover
  assert fires.

### PRODUCTION FIX (found by the extract wrapper test)

The zero-leftover asserts (`step_filters` end + `joint_filter_phase` end) fired on
**every** extract run with a real DB: `apply_filter_result_extract` does not
advance `phase`, so rows legitimately stay `unarchived` until the post-pass
`global_mark_phase(ExtractFiltered)` — the assert was archive-only in intent.
Both asserts are now gated on `pv.is_archive_phase()`. Archive invariants are
unchanged (the wrapper `run` tests still exercise the real leftover assert for the
archive path); extract + user filters now completes.

## 6. Wrappers

- `archive/filter.rs`: `ingest_filters` → `get_filters_archive` rows (with
  `clear_internal_include`, `keep` takes id `-1`); `run(rt)` end-to-end → reason
  columns + `filtered` phase + zero `hashed` left; `run` with no user filters →
  fast path `(-1, 0)`.
- `unarchive/filter.rs`: `ingest_filters` → `ParseFilterBuffer` contents +
  `write_to_db`; empty buffer carries the internal catch-all; `run(rt, true)` fast
  path promotes all; `run(rt, true)` with user filters (id `-1`/`+1` off the id-0
  dummy) sets extract reason columns + `ExtractFiltered`.

## NOT TESTABLE (comment only)

- `joint_filter_phase` `fix_up_canonical_flag down == up`: direct `db.` call (no
  closure seam) and structurally guaranteed per stale cluster — only a bug inside
  that SQL could trip it.
- The extract-side `step_filters` leftover assert: now gated off for extract (see
  fix above); for archive it is live and exercised by the wrapper `run` test.

## Build / test

- `nix develop -c cargo test -p tar-dedup --lib filter` (29 tests) while
  iterating; full lib suite: **412 passed / 1 failed** (the pre-existing stale
  `db::meta::tests::clear_archive_meta_drops_only_byte_counters`; unrelated).
- Non-test `cargo build -p tar-dedup` clean; no new warnings from the touched
  files.
