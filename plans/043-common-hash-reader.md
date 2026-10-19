# Plan: common/hash.rs — shared streaming sha1 (HashingReader)

Status: **agreed (2026-10-17), implementation round. No new tests** (existing
hash/rehash/footer batteries validate behavior).

## Problem

Three near-identical streaming-sha1-of-a-file functions exist:
`archive/hash.rs::hash_one` (whole file + interleaved zero-page scan + shutdown +
bar), `unarchive/rehash.rs::hash_file` (whole file + shutdown + bar), and
`archive_footer.rs::hash_archive_bytes` (prefix `[0..len)` + bar). Same core, triplicated.

## Core: `HashingReader<R: Read>` in `crates/tar-dedup/src/common/hash.rs`

- `struct HashingReader<R> { inner: R, hasher: Sha1 }`
- `impl<R: Read> Read`: read through inner, `hasher.update(&buf[..n])`, return `n`
  (transparent transform — the caller still sees the bytes).
- `finalize(self) -> [u8; 20]`: consume and return the digest.
- Owns no buffer; generic over `R`, so each caller keeps its own loop shape
  (zero-scan / shutdown / prefix limit / bar).

## Call-site refactors (each keeps its existing loop; behavior unchanged)

1. `archive_footer.rs::hash_archive_bytes`: read `[0..len)` through
   `HashingReader<File>`, feed the bar, `finalize()`.
2. `unarchive/rehash.rs::hash_file`: `File` → `HashingReader<File>`; loop keeps
   shutdown check + bar, returns `reader.finalize()`.
3. `archive/hash.rs::hash_one`: `File` → `HashingReader<File>`; loop keeps the
   zero-page scan (on the same `buf[..n]`) + shutdown + bar, returns
   `(reader.finalize(), zero_blocks)`.

Register `pub mod hash;` in `common.rs`.

## Verify

- `nix develop -c cargo build -p tar-dedup`
- `nix develop -c cargo test -p tar-dedup --lib` — existing `archive::hash`,
  `unarchive::rehash`, and footer round-trip tests must stay green (read→scan→bar→
  shutdown order is preserved).
