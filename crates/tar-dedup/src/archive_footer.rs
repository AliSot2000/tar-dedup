//! Seekable sqlite trailer appended after a finished compressed archive stream.
//!
//! Layout (file absolute):
//! `MAGIC | sha1(archive[0..offset)) | xz_bytes(-9e) | sha1(xz_bytes) | MAGIC | offset_u64_le`
//! where `offset` points at the first `MAGIC`. The first digest covers the
//! finished compressed archive (bytes before the footer); it is verified once at
//! extract install and downgraded to a warning under `--force`. The second digest
//! covers the compressed catalog blob. Layout is NOT backwards compatible before
//! v1.0.0 (no production archives exist yet).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use indicatif::ProgressBar;
use sha1::{Digest, Sha1};

use crate::common::defaults::ARCHIVE_IO_BUF_SIZE;
use crate::common::hash::HashingReader;
use crate::compression::{compress_footer_bytes, decompress_footer_bytes};
use crate::error::{Error, Result};

pub const FOOTER_MAGIC: &[u8] = b"Tar-Dedup-SQLite-Footer";
const SHA1_LEN: u64 = 20;
const OFFSET_LEN: u64 = 8;

fn footer_fixed_overhead() -> u64 {
    FOOTER_MAGIC.len() as u64 + SHA1_LEN + SHA1_LEN + FOOTER_MAGIC.len() as u64 + OFFSET_LEN
}

/// Stream-hash the first `len` bytes of `archive_path` with sha1, chunked at
/// `ARCHIVE_IO_BUF_SIZE`, reporting progress on `bar` when given.
fn hash_archive_bytes(archive_path: &Path, len: u64, bar: Option<&ProgressBar>)
    -> Result<[u8; 20]> {
    let file = File::open(archive_path).map_err(|e| Error::io(archive_path, e))?;
    if let Some(bar) = bar {
        bar.set_length(len);
        bar.set_position(0);
    }
    let mut reader = HashingReader::new(file);
    let mut buf = vec![0u8; ARCHIVE_IO_BUF_SIZE];
    let mut remaining = len;
    while remaining > 0 {
        let want = remaining.min(buf.len() as u64) as usize;
        let n = reader
            .read(&mut buf[..want])
            .map_err(|e| Error::io(archive_path, e))?;
        if n == 0 {
            break;
        }
        remaining -= n as u64;
        if let Some(bar) = bar {
            bar.inc(n as u64);
        }
    }
    Ok(reader.finalize())
}

/// Locate the footer in `file`: `Some(offset)` when a footer is present (trailing
/// MAGIC + valid offset + leading MAGIC at the offset), `None` when absent. The
/// leading-MAGIC mismatch after the presence gate is corruption, not absence.
fn locate_footer(archive_path: &Path, file: &mut File, len: u64) -> Result<Option<u64>> {
    let magic_len = FOOTER_MAGIC.len() as u64;
    let min_len = footer_fixed_overhead();
    if len < min_len {
        return Ok(None);
    }
    // Presence gate: trailing MAGIC (right before the u64 offset) + offset validity.
    file.seek(SeekFrom::Start(len - OFFSET_LEN - magic_len))
        .map_err(|e| Error::io(archive_path, e))?;
    let mut magic = vec![0u8; FOOTER_MAGIC.len()];
    file.read_exact(&mut magic)
        .map_err(|e| Error::io(archive_path, e))?;
    if magic != FOOTER_MAGIC {
        return Ok(None);
    }

    // Parse the offset from the last 8 bytes.
    file.seek(SeekFrom::Start(len - OFFSET_LEN))
        .map_err(|e| Error::io(archive_path, e))?;
    let mut off_buf = [0u8; 8];
    file.read_exact(&mut off_buf)
        .map_err(|e| Error::io(archive_path, e))?;
    let offset = u64::from_le_bytes(off_buf);
    if offset.checked_add(min_len).is_none_or(|end| end > len) {
        return Ok(None);
    }

    // Parse the first MAGIC Bytes.
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| Error::io(archive_path, e))?;
    file.read_exact(&mut magic)
        .map_err(|e| Error::io(archive_path, e))?;
    if magic != FOOTER_MAGIC {
        return Err(Error::Config(format!(
            "footer magic mismatch at offset {offset} in {}",
            archive_path.display()
        )));
    }
    Ok(Some(offset))
}

/// Append footer after the compression stream has closed and the work DB is finalized.
/// `bar`, when given, reports the progress of hashing the finished compressed archive.
pub fn write_footer(
    archive_path: &Path,
    sqlite_path: &Path,
    bar: Option<&ProgressBar>)
    -> Result<()> {
    let mut db_bytes = Vec::new();
    File::open(sqlite_path)
        .and_then(|mut f| f.read_to_end(&mut db_bytes))
        .map_err(|e| Error::io(sqlite_path, e))?;

    let xz_db_bytes = compress_footer_bytes(&db_bytes)?;
    let offset = std::fs::metadata(archive_path)
        .map_err(|e| Error::io(archive_path, e))?
        .len();
    // Integrity of the finished compressed archive: hash [0..offset).
    let content_digest = hash_archive_bytes(archive_path, offset, bar)?;
    let xz_db_digest = Sha1::digest(&xz_db_bytes);

    let mut out = OpenOptions::new()
        .append(true)
        .open(archive_path)
        .map_err(|e| Error::io(archive_path, e))?;

    out.write_all(FOOTER_MAGIC)
        .and_then(|_| out.write_all(&content_digest))
        .and_then(|_| out.write_all(&xz_db_bytes))
        .and_then(|_| out.write_all(&xz_db_digest))
        .and_then(|_| out.write_all(FOOTER_MAGIC))
        .and_then(|_| out.write_all(&offset.to_le_bytes()))
        .and_then(|_| out.sync_all())
        .map_err(|e| Error::io(archive_path, e))?;
    Ok(())
}

/// True when `archive_path` exists and carries a valid seekable sqlite footer.
pub fn has_valid_footer(archive_path: &Path) -> bool {
    if !archive_path.is_file() {
        return false;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!(
        "tar-dedup-footer-check-{}-{nanos}.sqlite",
        std::process::id()
    ));
    let ok = matches!(read_footer(archive_path, &tmp), Ok(true));
    let _ = std::fs::remove_file(&tmp);
    ok
}

/// Extract the footer sqlite to `dest` if a valid footer is present.
///
/// Returns `Ok(true)` when a valid footer was present and extracted to `dest`,
/// `Ok(false)` when no footer is present, and `Err(_)` when a footer structure is
/// present but failed to extract (corrupt magic/sha1/xz, decompress, or I/O).
pub fn read_footer(archive_path: &Path, dest: &Path) -> Result<bool> {
    let mut file = File::open(archive_path).map_err(|e| Error::io(archive_path, e))?;
    let len = file
        .metadata()
        .map_err(|e| Error::io(archive_path, e))?
        .len();
    let Some(offset) = locate_footer(archive_path, &mut file, len)? else {
        return Ok(false);
    };
    let magic_len = FOOTER_MAGIC.len() as u64;

    // Skip the content integrity digest (verified separately at extract install).
    file.seek(SeekFrom::Start(offset + magic_len + SHA1_LEN))
        .map_err(|e| Error::io(archive_path, e))?;

    // Read xz stream
    let xz_len = len
        .checked_sub(offset + magic_len + SHA1_LEN + SHA1_LEN + magic_len + OFFSET_LEN)
        .ok_or_else(|| Error::Config("footer size underflow".into()))?;
    let mut xz_bytes = vec![0u8; xz_len as usize];
    file.read_exact(&mut xz_bytes)
        .map_err(|e| Error::io(archive_path, e))?;

    // Check SHA of xz stream
    let mut digest = [0u8; 20];
    file.read_exact(&mut digest)
        .map_err(|e| Error::io(archive_path, e))?;
    let expected = Sha1::digest(&xz_bytes);
    if digest != expected.as_slice() {
        return Err(Error::Config(format!(
            "footer xz sha1 mismatch in {}",
            archive_path.display()
        )));
    }

    // Decompress opaque catalog blob; sqlite validity is checked when the DB is opened.
    let db_bytes = decompress_footer_bytes(&xz_bytes)?;

    // Create parent and write extracted db to disk.
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    std::fs::write(dest, &db_bytes).map_err(|e| Error::io(dest, e))?;
    Ok(true)
}

/// Verify the sha1 of the finished compressed archive content stored in the footer.
///
/// Returns `Ok(true)` when the stored digest matches `archive[0..offset)`,
/// `Ok(false)` when it does not (content corruption or no footer), and `Err` when
/// the footer structure cannot be read.
pub fn verify_archive_integrity(archive_path: &Path) -> Result<bool> {
    let mut file = File::open(archive_path).map_err(|e| Error::io(archive_path, e))?;
    let len = file
        .metadata()
        .map_err(|e| Error::io(archive_path, e))?
        .len();
    let Some(offset) = locate_footer(archive_path, &mut file, len)? else {
        return Ok(false);
    };
    // Cursor sits right after the leading MAGIC: read the stored content digest.
    let mut stored = [0u8; 20];
    file.read_exact(&mut stored)
        .map_err(|e| Error::io(archive_path, e))?;
    let actual = hash_archive_bytes(archive_path, offset, None)?;
    Ok(actual == stored)
}

/// Convenience: extract footer DB next to the archive under a given name.
/// Returns `None` when the archive carries no footer.
pub fn extract_footer_db(archive_path: &Path, dest_dir: &Path) -> Result<Option<PathBuf>> {
    let dest = dest_dir.join("footer-snapshot.sqlite");
    if read_footer(archive_path, &dest)? {
        Ok(Some(dest))
    } else {
        Ok(None)
    }
}
