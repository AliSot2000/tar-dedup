# 071 — Show progress + opt-out for archive-content verification at extract

Status: **implemented (2026-11-06).** Both A (bar reuse) and B (opt-out, default on) shipped; full lib suite + smoke verified. Remaining: VM `--features test-all` run (VM off at landing; run when booted). Aesthetic + UX fix for the first
`verify_archive_integrity` pass during extract: it hashes the entire compressed
archive with no progress (looks frozen on 100s of GiBs), and it is not skippable
even though extraction re-verifies every file's content.

## Problem

`archive_footer.rs::verify_archive_integrity(archive_path)` (called once at footer
install in `unarchive/scan.rs:231`) hashes `archive[0..offset)` via
`hash_archive_bytes(…, None)` — `None` bar. Meanwhile scan's single
`"extracting"` sub-bar (`scan.rs:184`, `BarKind::Bytes`) is alive and unused at
that point. The hash looks like a hang. There is also no way to skip it, although
extract already hashes each extracted file when `rehash` is on (default).

## Design

### A — reuse the `"extracting"` sub-bar for verification

`archive_footer.rs`:
- Change `verify_archive_integrity(archive_path: &Path)` →
  `verify_archive_integrity(archive_path: &Path, bar: Option<&ProgressBar>)`,
  threading `bar` through to `hash_archive_bytes` (which sizes/positions the bar
  to the content length). Mirrors `verify_footer`.

`unarchive/scan.rs` (at the `footer_this_pass` verify):
- Call `verify_archive_integrity(&rt.config.paths.archive_path, Some(&rt.pb))`.
- Set `rt.pb.set_message("verifying archive")` before the call, restore
  `"extracting"` after — the bar is shared with per-file extraction which sets
  its own message/length later.
- The sub-bar's `BarKind::Bytes` template (`{msg} [{bar}] {bytes}/{total} @
  {bytes_per_sec}`) now shows live progress over the whole archive.

Update the `verify_archive_integrity(…)` test call sites in
`archive_footer.rs` to pass `None` (identical behavior to today).

### B — opt-out flag `--no-verify-archive` (default on)

Rationale: the archive-content sha1 and per-file rehash are not strictly the same
check (archive-stream/structural vs extracted-content), so keep the fail-fast
whole-archive check on by default; let users who trust the archive or rely on
rehash skip the full read.

- `cli.rs` (`ExtractArgs`): add `--no-verify-archive` (`ArgAction::SetTrue`,
  `help_heading = "Scan"`).
- `config/extract.rs`: add `verify_archive: bool` (default `true`), merge via
  `merge_pick_copy`, build as `!args.no_verify_archive`.
- `scan.rs`: gate the verification block with `if rt.config.verify_archive { … }`.

## Files touched
- `crates/tar-dedup-lib/src/archive_footer.rs` (signature + bar threading + tests)
- `crates/tar-dedup-lib/src/unarchive/scan.rs` (bar reuse + gate)
- `crates/tar-dedup-lib/src/cli.rs` (`--no-verify-archive`)
- `crates/tar-dedup-lib/src/config/extract.rs` (field + merge + build)
- `docs/manual.md` (regenerate option tables via `scripts/gen_readme.py`)

## Open question
- Default of the flag: **opt-out** (`--no-verify-archive`, default on) — the
  recommended, safety-preserving choice; or **opt-in** (default off) if the
  redundant read is deemed not worth it. Confirmed to go with opt-out.

## Tests
- `archive_footer.rs`: existing `verify_archive_integrity_*` tests updated to the
  new signature (`None`); a `Some(&bar)` variant verifies the bar receives the
  content length (no regression in the verdict).
- `config/extract.rs`: `verify_archive` default true, `--no-verify-archive`
  flips it off; merge picks the override.
- `scan.rs` round-trip: with `verify_archive = true` a corrupt archive still
  fails/records; with `false` it is skipped (extraction proceeds) — exercise via
  the existing scan corrupt-archive test.
- Full `--features test-all` suite on the VM.