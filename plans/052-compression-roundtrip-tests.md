# Plan: compression/decompression round-trip tests (gated)

Status: **agreed (2026-10-22).** Today the compressor/decompressor stack has zero test
coverage (`tar_writer.rs`, `tar_reader.rs`, `compression/xz.rs`, `config/compression.rs`
have no `#[cfg(test)]`), and the footer xz path is never directly tested. We rely on the
codec crates and "hopes and prayers". This adds a feature-gated test suite.

## Gating

Everything below is compiled only under `#[cfg(feature = "test-compression")]`.
`crates/tar-dedup/Cargo.toml` `[features]` (root is a virtual workspace):

```toml
test-compression = []
test-all = ["test-compression"]   # meta feature for future heavy suites
```

Run: `cargo test -p tar-dedup --lib --features test-all`. Default `cargo test` compiles
none of it.

## Shared test support (`crates/tar-dedup/src/test_support.rs`, gated)

`#[cfg(feature = "test-compression")]` block with:
- `pub const TEST_PAYLOAD_SIZE_BYTES: usize = 32 * 1024;` — the single knob for payload size.
- `pub fn pseudo_random(len: usize) -> Vec<u8>` — deterministic `DefaultHasher`-fed stream (no new deps).
- `pub fn encode_bytes(data, format, level) -> Vec<u8>` — pure-bytes encoder building the
  *same* codec objects production `CompressLayer` uses (flate2 `GzEncoder`, `BzEncoder`,
  `zstd::stream::write::Encoder`, xz `Stream::new_easy_encoder`), single-threaded → deterministic.
- `pub fn all_levels() -> Vec<(CompressionFormat, u32, bool /*extreme*/)>` — Xz 0–9, Gz 1–9,
  Bz2 1–9, Zstd 1–19, + xz extreme axis 0e–9e (orthogonal bit 31, `LZMA_PRESET_LEVEL_MASK=0x1f`,
  `LZMA_PRESET_EXTREME=1<<31`), + `None` baseline.

## Codec-stage test (`tar_writer.rs::tests`, gated)

`compress → decompress → compress` on a **pseudorandom** stream (worst case for codec
correctness), per compressing format at its default level:
- C1 = `encode_bytes(R)`
- R1 = decode C1 via the production reader (`open_decompressed`) → assert `R1 == R`
- C2 = `encode_bytes(R1)` → assert `C2 == C1` (codec determinism)
Also covers the empty input.

## Full-stack test (`tar_writer.rs::tests`, gated)

`tar → TarSink → BufferedCompress → CompressLayer → file`, then back. Matrix over
`all_levels()` (58 combos). Per combo (label in assert messages):
1. Write sample files (empty, small text, ~`TEST_PAYLOAD_SIZE_BYTES` compressible,
   ~`TEST_PAYLOAD_SIZE_BYTES` pseudorandom).
2. `TarWriter::open(settings, jobs=1, Shutdown::detached(), sparse=false)` → `append_path`
   each → `finalize_archive()` → A1.
3. Write A2 from the **same untouched** files → **assert A1 == A2 byte-identical**
   (full-stack determinism — the only sensible check on the encoded file).
4. `open_tar_archive(A1)` → decompress → **assert every member's bytes == source file**
   (content-only; no round-trip cycle here).

## Footer (`compression/xz.rs::tests`, gated)

`decompress_footer_bytes(compress_footer_bytes(x)) == x` for empty + ~1 MiB pseudorandom.
(Fixed `FOOTER_XZ_PRESET` = `9 | EXTREME`.)

## Compressed + footer integration (`archive_footer.rs::tests`, gated)

Write a small xz archive via `TarWriter`, append the sqlite footer via `write_footer`, then:
`verify_archive_integrity` true, `read_footer` true, and `open_tar_archive` still yields the
member content (footer logic on trailing data after a compressed stream).

## Config validation (`config/compression.rs::tests`, gated)

- `level_range` / `default_level` for all 5 formats.
- `infer_compression_from_suffix` (~12 suffix cases) and `resolve_compression`
  (flag conflict, flag-beats-suffix, bare suffix → `None`; `CompressionFlags` constructed
  directly — its fields are `pub`).
- `CompressionSettings::from_archive_args` via `Cli::try_parse_from([…, "archive", …])`
  (pattern proven in `cli.rs`): out-of-range level per format, `--level` without a filter,
  `--xz-extreme`/`--memlimit-compress` on non-xz → `Err(Config)`; boundary + default accept.

## Files touched

- `crates/tar-dedup/Cargo.toml` (features)
- `crates/tar-dedup/src/test_support.rs` (gated helpers)
- `crates/tar-dedup/src/tar_writer.rs` (gated tests)
- `crates/tar-dedup/src/compression/xz.rs` (gated tests)
- `crates/tar-dedup/src/archive_footer.rs` (gated test)
- `crates/tar-dedup/src/config/compression.rs` (gated tests)
- `plans/052-compression-roundtrip-tests.md`

## Verification

- `cargo test -p tar-dedup --lib --features test-all` (gated suite; ~58 codec cycles,
  slowest xz 9e — on-demand only)
- `cargo test -p tar-dedup --lib` (default suite unchanged; expect only the pre-existing
  `clear_archive_meta_drops_only_byte_counters` failure)

## Implementation notes (2026-10-22)

- All 9 tests implemented. Gated suite: **488 passed / 1 failed** (only the pre-existing
  blocker). Default suite unchanged: 479 passed / 1 failed.
- `TarWriter::open` opens the archive with `append(true)`: the matrix therefore gives each
  combo its own `a1-{i}.bin` / `a2-{i}.bin` path, or late combos read back bytes spilled
  by earlier formats ("invalid gzip header" during a Gz read-back was the tell).
- **Determinism holds for the full 58-combo matrix** (incl. xz MT encoder at jobs=1 and
  zstd 1–19): `A1 == A2` byte-identical everywhere — the risk-note fallback (weakening to
  decompressed-stream equality) was not needed.
- `all_levels()` covers Xz 0–9, Gz 1–9, Bz2 1–9, Zstd 1–19, Xz extreme 0e–9e, `None` = 58.
- Codec-stage test runs the compressing formats at default level + empty input; determinism
  across every level is still exercised by the full-stack matrix.