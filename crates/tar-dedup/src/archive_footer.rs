//! Seekable sqlite trailer appended after a finished compressed archive stream.
//!
//! Layout (file absolute):
//! `MAGIC | xz_bytes(-9e) | sha1(xz_bytes) | MAGIC | offset_u64_le`
//! where `offset` points at the first `MAGIC`.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use crate::compression::{compress_footer_bytes, decompress_footer_bytes};
use crate::error::{Error, Result};

pub const FOOTER_MAGIC: &[u8] = b"Tar-Dedup-SQLite-Footer";
const SHA1_LEN: u64 = 20;
const OFFSET_LEN: u64 = 8;

fn footer_fixed_overhead() -> u64 {
    FOOTER_MAGIC.len() as u64 + SHA1_LEN + FOOTER_MAGIC.len() as u64 + OFFSET_LEN
}

/// Append footer after the compression stream has closed and the work DB is finalized.
pub fn write_footer(archive_path: &Path, sqlite_path: &Path) -> Result<()> {
    let mut db_bytes = Vec::new();
    File::open(sqlite_path)
        .and_then(|mut f| f.read_to_end(&mut db_bytes))
        .map_err(|e| Error::io(sqlite_path, e))?;

    let xz_bytes = compress_footer_bytes(&db_bytes)?;
    let digest = Sha1::digest(&xz_bytes);
    let offset = std::fs::metadata(archive_path)
        .map_err(|e| Error::io(archive_path, e))?
        .len();

    let mut out = OpenOptions::new()
        .append(true)
        .open(archive_path)
        .map_err(|e| Error::io(archive_path, e))?;

    out.write_all(FOOTER_MAGIC)
        .and_then(|_| out.write_all(&xz_bytes))
        .and_then(|_| out.write_all(&digest))
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
    let ok = read_footer(archive_path, &tmp).is_ok();
    let _ = std::fs::remove_file(&tmp);
    ok
}

/// Extract the footer sqlite to `dest` if a valid footer is present.
pub fn read_footer(archive_path: &Path, dest: &Path) -> Result<()> {
    // Get file length and check if it's long enough for valid footer.
    let mut file = File::open(archive_path).map_err(|e| Error::io(archive_path, e))?;
    let len = file
        .metadata()
        .map_err(|e| Error::io(archive_path, e))?
        .len();
    let magic_len = FOOTER_MAGIC.len() as u64;
    let min_len = footer_fixed_overhead();
    if len < min_len {
        return Err(Error::Config(format!(
            "archive too small for footer: {}",
            archive_path.display()
        )));
    }

    // Get offset and validate it.
    file.seek(SeekFrom::End(-(OFFSET_LEN as i64)))
        .map_err(|e| Error::io(archive_path, e))?;
    let mut off_buf = [0u8; 8];
    file.read_exact(&mut off_buf)
        .map_err(|e| Error::io(archive_path, e))?;
    let offset = u64::from_le_bytes(off_buf);
    if offset + min_len > len {
        return Err(Error::Config(format!(
            "invalid footer offset {offset} in {}",
            archive_path.display()
        )));
    }

    // Check Magic is first
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| Error::io(archive_path, e))?;
    let mut magic = vec![0u8; FOOTER_MAGIC.len()];
    file.read_exact(&mut magic)
        .map_err(|e| Error::io(archive_path, e))?;
    if magic != FOOTER_MAGIC {
        return Err(Error::Config(format!(
            "footer magic mismatch at offset {offset} in {}",
            archive_path.display()
        )));
    }

    // Read xz stream
    let xz_len = len
        .checked_sub(offset + magic_len + SHA1_LEN + magic_len + OFFSET_LEN)
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

    // Check file ends in footer (prior to the u64 for the offset)
    file.read_exact(&mut magic)
        .map_err(|e| Error::io(archive_path, e))?;
    if magic != FOOTER_MAGIC {
        return Err(Error::Config(format!(
            "trailing footer magic mismatch in {}",
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
    Ok(())
}

/// Convenience: extract footer DB next to the archive under a given name.
pub fn extract_footer_db(archive_path: &Path, dest_dir: &Path) -> Result<PathBuf> {
    let dest = dest_dir.join("footer-snapshot.sqlite");
    read_footer(archive_path, &dest)?;
    Ok(dest)
}
