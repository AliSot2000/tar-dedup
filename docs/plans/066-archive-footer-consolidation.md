# 066 — Consolidate `archive_footer.rs` hashing + trailer parsing

Status: approved in conversation (build).

## Goal

Two near-identical hashing loops and the trailer layout math duplicated across
three read paths. Merge them so `archive_footer.rs` has a single hashing core and
a single place that parses the seekable footer trailer — behavior-neutral.

## M1 — one hashing core

- `fn hash_section(path, start, len, bar) -> Result<[u8; 20]>` (private): the
  shared loop (open → `seek(start)` → `HashingReader` → chunked read at
  `ARCHIVE_IO_BUF_SIZE` → `bar.inc` → `finalize`).
- `fn hash_archive_bytes(path, len, bar)` becomes a thin wrapper: size/position the
  bar, then `hash_section(path, 0, len, bar)`. Kept for `write_footer` and
  `verify_archive_integrity`.
- `verify_footer` calls `hash_section` directly (bar pre-sized to
  `offset + xz_len` for the two passes).

## M2 — shared trailer-layout parse

```rust
struct FooterLayout {
    offset: u64,          // trailer start (leading MAGIC)
    content_digest: [u8; 20],  // sha1(archive[0..offset))
    xz_start: u64,        // offset + MAGIC + SHA1_LEN
    xz_len: u64,          // stored xz catalog blob length
    xz_digest: [u8; 20],  // sha1(xz blob), read at xz_start + xz_len
}

impl FooterLayout {
    /// Ok(None) = no footer; Err(Config) = corrupt; Err(io) = filesystem.
    fn from_file(path: &Path, file: &mut File, len: u64) -> Result<Option<Self>>
}
```

Constructor: `locate_footer` → read `content_digest` (cursor already right after
the leading MAGIC) → compute `xz_start` / `xz_len` → read `xz_digest` at
`xz_start + xz_len`.

**Underflow handling** (per direction): `locate_footer`'s presence gate already
guarantees `offset + fixed ≤ len`, so `xz_len` can never underflow — replace the
`ok_or_else(Error::Config("footer size underflow"))` with

```rust
debug_assert!(offset.saturating_add(footer_fixed_overhead()) <= len,
              "PRECONDITION: locate_footer ensures offset + fixed <= len");
let xz_len = len - (offset + footer_fixed_overhead());
```

(Precise gate invariant is `<=`; `locate_footer` rejects only `end > len`.)

## M3 — consumers

- `verify_footer`: `Err(Config) → InvalidFooter`, `None → NoFooter`;
  `hash_section(0, offset, bar) == content_digest`,
  `hash_section(xz_start, xz_len, bar) == xz_digest` → the six verdicts.
- `read_footer`: read `[xz_start, xz_start+xz_len)`, compare `xz_digest`,
  decompress, write (digest read at `xz_start + xz_len` equals today's
  read-after-blob).
- `verify_archive_integrity`: content-only via the same layout.
- `write_footer` untouched (uses the `hash_archive_bytes` wrapper).

## Verification

- Existing footer test battery unchanged and green (incl. all six
  `verify_footer_*`, `read_footer_roundtrip…`, `compressed_stream_with_footer_reads_back`).
- `cargo test -p tar-dedup-lib --lib` (722) and the end-to-end `check` verdicts
  re-run.

## Files

`crates/tar-dedup-lib/src/archive_footer.rs` + this plan.