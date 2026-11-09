# 084 — read_probe uses full content id; drop the sha-less content-id path

Status: **approved — implementing.**

## Motivation

`--retry-missing-sha` was dropped (`b1435a65`); the `;`-fill `None`-digest path in the
content-id encoder was its only consumer. The runtime pipeline already guarantees every
content id carries a digest (`db/stage.rs`, `db/tar_writer.rs`, `db/rehash.rs` all filter
`sha1 IS NOT NULL`). We revert the API to require a digest and teach the bypass
`read_probe` to consume the full parsed content id.

## 1. `content_id` requires a digest (`db/content_id.rs`, `db/common.rs`)

- `content_id_from_digest(digest: &[u8; 20], size, file_id, rel_path)` — drop the `Option` and
  the `";".repeat(27)` fill.
- `parse_content_id(...) -> ([u8; 20], u64, FileId, String)` — drop the `;`→`None` branch; an
  all-`;` (or any invalid/absent) hash now fails `parse_content_id`.
- `db/common.rs::content_id_if_canonical` — `let sha1 = sha1?;` so a sha-less canonical row
  returns `None` (fixes a latent bug: `db/place.rs:334` already documents `content_id() = Some`
  ⇔ "digest is known").

## 2. `read_probe` consumes the full content id (`unarchive/scan.rs`)

```rust
fn read_probe(path: &Path, digest: &[u8; 20], expected_len: u64, buf: &mut Vec<u8>)
    -> io::Result<bool>
```

The caller destructures the non-optional `Ok((digest, size, fid, _))`, so "the digest must be
set" becomes a compile-time contract. Body unchanged: stream through `buf`, return
`total == expected_len` (the size from the name).

## 3. Call sites + tests

- Runtime `parse_content_id` users already drop the digest (`db/scan.rs:56`,
  `unarchive/scan.rs:731`) — type-only adjustment.
- Builders/tests passing `Some(&[x;20])`: `unarchive/place.rs:1269,2444,2461,2486`,
  `test_support.rs:562` → drop `Some`.
- `content_id.rs` tests: `absent_digest_parses_to_none` removed; add a test that an
  all-`;`/invalid hash name is rejected; round-trip tests drop the `Some`.
- `unarchive/permissions.rs:657` doc-comment referencing the `;` fill → update.

## 4. Verification

- `cargo test -p tar-dedup-lib --lib` (+ `--features test-compression`).
- VM: sync, `cargo test --features test-all`.
- (Note: `unarchive.rs` is mid-rework by the author; ask before running tests.)