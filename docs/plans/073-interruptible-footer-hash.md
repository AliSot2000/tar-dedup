# 073 — Interruptible + unified range hashing / resumable footer step

Status: **agreed (2026-11-06)**, implementation pending (context will be
compressed after this file is written; resume from here).

## Problem

The footer's content-integrity hash (`archive_footer::hash_section`) reads the
whole finished compressed archive back with **no interrupt check and no shared
loop** — so on 100s of GiB the write-time footer hash and the extract/`check`
verify cannot be interrupted, and an abort mid-write leaves a messy resume. The
digest primitive (`HashingReader`) is already shared, but the two read loops
(files vs footer) differ.

## Context: footer build order

Content + final `snapshot.sqlite` are written first; `finalize_archive()` closes
the tar; the DB session is finalized + `checkpoint`ed; then `write_footer`
**re-reads** `archive[0..offset)`, hashes it, and appends the trailer. So an
interrupt during the footer hash lands with: `has_finalized_archive_session() ==
true`, `archive_len == archive_bytes_out`, footer missing, phase still
`archive`. Recovery never truncates this (it only touches *open* sessions).

## A. Single source of truth — `common/hash.rs`

Add (on top of `HashingReader`), mirroring `archive/hash.rs::hash_one`'s
interrupt pattern:

```rust
pub fn hash_file_range(path: &Path, start: u64, len: u64,
                       shutdown: &Shutdown, bar: Option<&ProgressBar>)
    -> Result<[u8; 20]>
```
- per-chunk `shutdown.check_in_flight()?`, then `bar.inc(n)`.
- `archive_footer::hash_section` / `hash_archive_bytes` become thin delegates.
  Bar sizing stays at the exact call sites (`write_footer`: `[0..offset)`;
  `verify_footer`: sizes to `offset + xz_len` across two passes).

## B. Write-time interrupt + resumable footer step (derived)

- Thread `&Shutdown` into `write_footer`/`hash_archive_bytes`/`hash_section`.
  On `Error::Interrupted` from the range hash, `end_session` propagates it →
  `tar_builder::run` returns `Interrupted` → phase stays `archive`. Archive left
  complete-but-footerless (valid tar; extractable via leading-manifest scan).
- New **first** branch in `tar_builder::run()` (before
  `check_archive_bytes_out` / `begin_archive_session`):

  ```rust
  if rt.config.pipeline.write_archive_footer
      && !archive_footer::has_valid_footer(&rt.config.paths.archive_path)
      && rt.db.has_finalized_archive_session()?
  {
      // Stream already finalized; only the trailer is missing.
      run_footer_one_shot(rt)?;   // re-hash (bar) + append footer
      return Ok(());              // phase advances → cleanup/done
  }
  ```
- **Derived** (no schema change): footer-pending ⇔ (footer enabled ∧ no valid
  footer ∧ finalized session). `has_valid_footer` is already the canonical
  footer-presence predicate (used by `archive.rs` ProductPresence + installer),
  so no second source of truth. Guards: only when footer is configured, and the
  branch runs before any new session/compression.
- `archive_bytes_out` remains the tar-content length (footer is out-of-band,
  consistent with `check_archive_bytes_out`).

## C. Extract + `check` interruptible (consistency)

- `scan.rs`: pass `rt.shutdown` to `verify_archive_integrity`; on interrupt →
  `Err(Interrupted)` → extract resumable, archive untouched.
- `cmd/check.rs`: supply a `Shutdown` so `--check` aborts cleanly on Ctrl-C.

## Files touched
- `crates/tar-dedup-lib/src/common/hash.rs` (new `hash_file_range`)
- `crates/tar-dedup-lib/src/archive_footer.rs` (thread `&Shutdown`; delegate to helper)
- `crates/tar-dedup-lib/src/archive/tar_builder.rs` (footer one-shot resume branch + call)
- `crates/tar-dedup-lib/src/unarchive/scan.rs` (verify passes shutdown)
- `crates/tar-dedup-lib/src/cmd/check.rs` (shutdown for `verify_footer`)
- `crates/tar-dedup-lib/src/test_support.rs` + archive_footer tests (detached `Shutdown`)

## Tests
- `common/hash.rs`: range correctness (known bytes → expected sha1); interrupt
  mid-range → `Err(Interrupted)`; `bar` progress increments.
- `archive_footer.rs` / `tar_builder.rs`: write-footer interrupt → footerless
  finalized → **resume one-shot appends footer**; `has_valid_footer` true, bytes
  intact, phase advances.
- `scan.rs`: verify-interrupt test.
- Full lib + `--features test-all` on the VM.