//! Seekable sqlite trailer appended after a finished compressed archive stream.
//!
//! Layout (file absolute):
//! `MAGIC | sqlite_bytes | sha1(20) | MAGIC | offset_u64_le`
//! where `offset` points at the first `MAGIC`.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use sha1::{Digest, Sha1};

use crate::error::{Error, Result};

pub const FOOTER_MAGIC: &[u8] = b"Tar-Dedup-SQLite-Footer";

/// Append footer after the compression stream has closed and the work DB is finalized.
pub fn write_footer(archive_path: &Path, sqlite_path: &Path) -> Result<()> {
    let mut db_bytes = Vec::new();
    File::open(sqlite_path)
        .and_then(|mut f| f.read_to_end(&mut db_bytes))
        .map_err(|e| Error::io(sqlite_path, e))?;

    let digest = Sha1::digest(&db_bytes);
    let offset = std::fs::metadata(archive_path)
        .map_err(|e| Error::io(archive_path, e))?
        .len();

    let mut out = OpenOptions::new()
        .append(true)
        .open(archive_path)
        .map_err(|e| Error::io(archive_path, e))?;

    out.write_all(FOOTER_MAGIC)
        .and_then(|_| out.write_all(&db_bytes))
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
    let min_len = magic_len + 20 + magic_len + 8;
    if len < min_len {
        return Err(Error::Config(format!(
            "archive too small for footer: {}",
            archive_path.display()
        )));
    }

    // Read the offset and sanity check size.
    file.seek(SeekFrom::End(-8))
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

    // Check for the presence of the magic string
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

    // Lift db
    let db_len = len
        .checked_sub(offset + magic_len + 20 + magic_len + 8)
        .ok_or_else(|| Error::Config("footer size underflow".into()))?;
    let mut db_bytes = vec![0u8; db_len as usize];
    file.read_exact(&mut db_bytes)
        .map_err(|e| Error::io(archive_path, e))?;

    // Check hash of db
    let mut digest = [0u8; 20];
    file.read_exact(&mut digest)
        .map_err(|e| Error::io(archive_path, e))?;
    let expected = Sha1::digest(&db_bytes);
    if digest != expected.as_slice() {
        return Err(Error::Config(format!(
            "footer sqlite sha1 mismatch in {}",
            archive_path.display()
        )));
    }

    // Check second footer
    file.read_exact(&mut magic)
        .map_err(|e| Error::io(archive_path, e))?;
    if magic != FOOTER_MAGIC {
        return Err(Error::Config(format!(
            "trailing footer magic mismatch in {}",
            archive_path.display()
        )));
    }

    // Write DB to file
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
