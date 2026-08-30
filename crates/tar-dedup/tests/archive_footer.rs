mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};
use tar_dedup::archive_footer::{self, FOOTER_MAGIC};
use tar_dedup::compression::{compress_footer_bytes, decompress_footer_bytes};
use tar_dedup::error::Error;

fn minimal_sqlite(path: &Path) {
    let abs = path
        .parent()
        .expect("sqlite parent")
        .join("a.txt")
        .to_string_lossy()
        .into_owned();
    common::write_archived_snapshot(path, &[&abs]);
}

fn archive_with_footer(work: &Path) -> (PathBuf, PathBuf) {
    let sqlite = work.join("catalog.sqlite");
    minimal_sqlite(&sqlite);

    let archive = work.join("archive.tar");
    fs::write(&archive, b"fake-tar-stream-prefix").expect("write archive prefix");
    archive_footer::write_footer(&archive, &sqlite).expect("write footer");
    (archive, sqlite)
}

fn read_file_bytes(path: &Path) -> Vec<u8> {
    fs::read(path).expect("read archive")
}

fn write_file_bytes(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).expect("write archive");
}

fn footer_offset(archive: &Path) -> u64 {
    let bytes = read_file_bytes(archive);
    u64::from_le_bytes(bytes[bytes.len() - 8..].try_into().unwrap())
}

/// What: `write_footer` → `read_footer` preserves sqlite bytes on a minimal catalog DB.
/// Why: Happy-path (a) among read paths — (a) round-trip, (b) sha1 fail, (c) magic fail, (d) xz fail, (e) non-sqlite payload.
/// Particularities: Archive must contain a prefix before the footer (simulates compressed tar); offset is from file start.
#[test]
fn footer_round_trip_minimal_sqlite() {
    let work = tempfile::tempdir().expect("tempdir");
    let (archive, sqlite) = archive_with_footer(work.path());

    let out = work.path().join("extracted.sqlite");
    archive_footer::read_footer(&archive, &out).expect("read footer");

    let original = fs::read(&sqlite).expect("read source sqlite");
    let extracted = fs::read(&out).expect("read extracted sqlite");
    assert_eq!(original, extracted);
    assert!(archive_footer::has_valid_footer(&archive));
}

/// What: `has_valid_footer` and `extract_footer_db` delegate to the same validation as `read_footer`.
/// Why: Covers public convenience wrappers (b) after happy-path (a).
/// Particularities: `extract_footer_db` writes `footer-snapshot.sqlite` under the given directory.
#[test]
fn has_valid_footer_and_extract_footer_db() {
    let work = tempfile::tempdir().expect("tempdir");
    let (archive, sqlite) = archive_with_footer(work.path());

    assert!(archive_footer::has_valid_footer(&archive));

    let dest_dir = work.path().join("out");
    fs::create_dir_all(&dest_dir).expect("mkdir");
    let extracted = archive_footer::extract_footer_db(&archive, &dest_dir)
        .expect("extract footer db");
    assert_eq!(extracted, dest_dir.join("footer-snapshot.sqlite"));
    assert_eq!(fs::read(&sqlite).unwrap(), fs::read(&extracted).unwrap());
}

/// What: Tampered sha1 over the xz blob is rejected before decompression.
/// Why: Integrity checks — (b) sha1 mismatch among (b) sha1, (c) magic, (d) xz.
/// Particularities: Digest covers the xz bytes only; flipping digest byte 0 at `offset + magic + xz_len` must fail.
#[test]
fn read_footer_rejects_xz_sha1_mismatch() {
    let work = tempfile::tempdir().expect("tempdir");
    let (archive, _) = archive_with_footer(work.path());
    let offset = footer_offset(&archive);

    let mut bytes = read_file_bytes(&archive);
    let magic_len = FOOTER_MAGIC.len();
    let xz_len = bytes.len() - offset as usize - magic_len - 20 - magic_len - 8;
    let digest_off = offset as usize + magic_len + xz_len;
    bytes[digest_off] ^= 0xff;

    write_file_bytes(&archive, &bytes);
    let err = archive_footer::read_footer(&archive, &work.path().join("out.sqlite"))
        .expect_err("sha1 mismatch");
    assert!(matches!(err, Error::Config(_)));
    assert!(err.to_string().contains("sha1 mismatch"));
}

/// What: Wrong leading `MAGIC` is rejected using the trailing offset.
/// Why: Magic checks — (c) leading magic among integrity cases.
/// Particularities: First byte of leading magic sits exactly at `offset`; offset u64 at EOF must still match.
#[test]
fn read_footer_rejects_leading_magic_mismatch() {
    let work = tempfile::tempdir().expect("tempdir");
    let (archive, _) = archive_with_footer(work.path());
    let offset = footer_offset(&archive);

    let mut bytes = read_file_bytes(&archive);
    bytes[offset as usize] ^= 0xff;

    write_file_bytes(&archive, &bytes);
    let err = archive_footer::read_footer(&archive, &work.path().join("out.sqlite"))
        .expect_err("leading magic");
    assert!(matches!(err, Error::Config(_)));
    assert!(err.to_string().contains("magic mismatch"));
}

/// What: Wrong trailing `MAGIC` is rejected after sha1 verification.
/// Why: Magic checks — (d) trailing magic among integrity cases.
/// Particularities: Trailing magic begins at `offset + magic_len + xz_len + 20`; sha1 must remain valid.
#[test]
fn read_footer_rejects_trailing_magic_mismatch() {
    let work = tempfile::tempdir().expect("tempdir");
    let (archive, _) = archive_with_footer(work.path());
    let offset = footer_offset(&archive);

    let mut bytes = read_file_bytes(&archive);
    let magic_len = FOOTER_MAGIC.len();
    let xz_len = bytes.len() - offset as usize - magic_len - 20 - magic_len - 8;
    let trailing_magic_off = offset as usize + magic_len + xz_len + 20;
    bytes[trailing_magic_off] ^= 0xff;

    write_file_bytes(&archive, &bytes);
    let err = archive_footer::read_footer(&archive, &work.path().join("out.sqlite"))
        .expect_err("trailing magic");
    assert!(matches!(err, Error::Config(_)));
    assert!(err.to_string().contains("trailing footer magic mismatch"));
}

/// What: Valid sha1 but corrupt xz stream fails during decompression.
/// Why: Integrity checks — (e) bad xz among cases; sha1 alone is insufficient.
/// Particularities: Flip a byte inside the xz blob and recompute sha1 over the modified xz bytes; error may be `Error::Other` from liblzma.
#[test]
fn read_footer_rejects_invalid_xz_payload() {
    let work = tempfile::tempdir().expect("tempdir");
    let (archive, _) = archive_with_footer(work.path());
    let offset = footer_offset(&archive);

    let mut bytes = read_file_bytes(&archive);
    let magic_len = FOOTER_MAGIC.len();
    let xz_off = offset as usize + magic_len;
    let xz_len = bytes.len() - offset as usize - magic_len - 20 - magic_len - 8;
    bytes[xz_off] ^= 0xff;
    let digest = Sha1::digest(&bytes[xz_off..xz_off + xz_len]);
    let digest_off = xz_off + xz_len;
    bytes[digest_off..digest_off + 20].copy_from_slice(digest.as_slice());

    write_file_bytes(&archive, &bytes);
    let err = archive_footer::read_footer(&archive, &work.path().join("out.sqlite"))
        .expect_err("invalid xz");
    assert!(
        err.to_string().contains("decompress") || matches!(err, Error::Config(_)),
        "unexpected error: {err}"
    );
}

/// What: Xz payload with arbitrary bytes (not sqlite-specific) round-trips through `read_footer`.
/// Why: Footer treats catalog bytes as opaque — (f) format independence after (e) xz decode.
/// Particularities: Must rebuild footer with xz(compress(payload)) and valid sha1 over that xz blob; sqlite open happens later.
#[test]
fn read_footer_accepts_opaque_payload() {
    let work = tempfile::tempdir().expect("tempdir");
    let archive = work.path().join("archive.tar");
    fs::write(&archive, b"prefix").expect("prefix");

    let payload = b"opaque-catalog-bytes-not-sqlite-specific";
    let xz_bytes = compress_footer_bytes(payload).expect("compress payload");
    let digest = Sha1::digest(&xz_bytes);
    let offset = fs::metadata(&archive).unwrap().len();

    let mut out = OpenOptions::new()
        .append(true)
        .open(&archive)
        .expect("open archive");
    out.write_all(FOOTER_MAGIC).unwrap();
    out.write_all(&xz_bytes).unwrap();
    out.write_all(&digest).unwrap();
    out.write_all(FOOTER_MAGIC).unwrap();
    out.write_all(&offset.to_le_bytes()).unwrap();

    let out_path = work.path().join("out.bin");
    archive_footer::read_footer(&archive, &out_path).expect("opaque payload");
    assert_eq!(fs::read(&out_path).unwrap(), payload);
}

/// What: `compress_footer_bytes` / `decompress_footer_bytes` round-trip arbitrary bytes.
/// Why: Isolates xz helper (g) from full footer layout; footer code assumes symmetric preset -10e.
/// Particularities: Uses the same preset as `write_footer`; decompressed size must match input exactly.
#[test]
fn footer_xz_helpers_round_trip() {
    let input = b"arbitrary-opaque-payload-for-xz-round-trip";
    let compressed = compress_footer_bytes(input).expect("compress");
    let decompressed = decompress_footer_bytes(&compressed).expect("decompress");
    assert_eq!(input.as_slice(), decompressed.as_slice());
}

/// What: Archive shorter than fixed footer overhead is rejected.
/// Why: Bounds checks — (h) minimum size among edge cases.
/// Particularities: Fixed overhead is `2 * magic_len + 20 + 8`; no xz payload required to trigger this path.
#[test]
fn read_footer_rejects_too_small_archive() {
    let work = tempfile::tempdir().expect("tempdir");
    let archive = work.path().join("tiny.tar");
    fs::write(&archive, b"short").expect("write tiny");

    let err = archive_footer::read_footer(&archive, &work.path().join("out.sqlite"))
        .expect_err("too small");
    assert!(matches!(err, Error::Config(_)));
    assert!(err.to_string().contains("too small"));
}

/// What: Trailing offset pointing past EOF is rejected.
/// Why: Bounds checks — (i) invalid offset among edge cases.
/// Particularities: File must be at least `footer_fixed_overhead` bytes or the too-small path fires first; offset must satisfy `offset + overhead > len`.
#[test]
fn read_footer_rejects_invalid_offset() {
    let work = tempfile::tempdir().expect("tempdir");
    let archive = work.path().join("bad-offset.tar");
    let overhead = FOOTER_MAGIC.len() * 2 + 20 + 8;
    let mut bytes = vec![0u8; overhead];
    bytes.extend_from_slice(b"prefix");
    let bogus_offset = bytes.len() as u64 + 100;
    bytes.extend_from_slice(&bogus_offset.to_le_bytes());
    fs::write(&archive, &bytes).expect("write");

    let err = archive_footer::read_footer(&archive, &work.path().join("out.sqlite"))
        .expect_err("invalid offset");
    assert!(matches!(err, Error::Config(_)));
    assert!(err.to_string().contains("invalid footer offset"));
}
