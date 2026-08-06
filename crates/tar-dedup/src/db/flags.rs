use rusqlite::{named_params, Connection};

use crate::db::types::FileId;
use crate::error::Result;

/// Bit index into [`FileFlags`] (not the mask itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum FileFlag {
    /// Payload for this content landed in the extract cache (canonical row only).
    /// Set after a successful `unpack`; cleared on catalog install normalization.
    FileExtracted = 0,
    /// File changed while the archive pipeline was touching it.
    Modified = 1,
    /// Sparse rewrite exists; stage/archive should use the sparsified target.
    HasSparse = 2,
    /// Compare vs this round's canonical finished; content differs.
    /// Cleared on round end for the whole `(sha1, size)` group.
    CheckWithCanonicalCompleted = 3,
    /// Read/compare failed during dedup. Sticky — never cleared on later success.
    /// Excludes the file from canonical election.
    ErrorWhileDedup = 4,
    /// Sparse copy failed (permissions, IO, …). Sticky.
    ErrorWhileSparsify = 5,
    /// Payload was written into an archive session.
    /// Set on successful `append_path`. Left standing when the session finalizes
    /// (`phase` → `archived`). Cleared only on abort/truncate for rows that are
    /// still not `archived` (incomplete session rewrite).
    AppendedPath = 6,
    /// `append_path` failed during the archive process.
    ErrorWhileArchive = 7,
    /// Rehashing Encountered a mismatch
    RehashMismatch = 8,
    /// While attempting the rehash, an error occurred
    ErrorWhileRehashing = 9,

    // TODO
    //  Error while unarchiving
    //  IO Error while indexing
    //  XATTR Error while indexing
    //  POSIX_ACL Error while indexing
    //  SELinux Error while indexing
}

impl FileFlag {
    pub const fn mask(self) -> u64 {
        1u64 << (self as u8)
    }

    pub const fn mask_i64(self) -> i64 {
        self.mask() as i64
    }
}

/// Bitset stored in `files.flags`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FileFlags(u64);

impl FileFlags {
    pub const fn from_bits(bits: u64) -> Self {
        // Keep sign bit clear for SQLite INTEGER round-trips.
        Self(bits & !(1u64 << 63))
    }

    pub const fn bits(self) -> u64 {
        self.0
    }

    pub fn from_i64(raw: i64) -> Self {
        Self::from_bits(raw as u64)
    }

    pub fn to_i64(self) -> i64 {
        self.0 as i64
    }

    pub fn get(self, flag: FileFlag) -> bool {
        self.0 & flag.mask() != 0
    }

    pub fn set(&mut self, flag: FileFlag, on: bool) {
        if on {
            self.0 |= flag.mask();
        } else {
            self.0 &= !flag.mask();
        }
    }

    pub fn with(mut self, flag: FileFlag, on: bool) -> Self {
        self.set(flag, on);
        self
    }
}

pub fn get_flags(conn: &Connection, file_id: FileId) -> Result<FileFlags> {
    let raw: i64 = conn.query_row(
        "SELECT flags FROM files WHERE id = :id",
        named_params! { ":id": file_id.0 },
        |row| row.get(0),
    )?;
    Ok(FileFlags::from_i64(raw))
}

pub fn set_flags(conn: &Connection, file_id: FileId, flags: FileFlags) -> Result<()> {
    conn.execute(
        "UPDATE files SET flags = :flags WHERE id = :id",
        named_params! {
            ":flags": flags.to_i64(),
            ":id": file_id.0,
        },
    )?;
    Ok(())
}

pub fn get_flag(conn: &Connection, file_id: FileId, flag: FileFlag) -> Result<bool> {
    let set: i64 = conn.query_row(
        "SELECT (flags & :bit) != 0 FROM files WHERE id = :id",
        named_params! {
            ":bit": flag.mask_i64(),
            ":id": file_id.0,
        },
        |row| row.get(0),
    )?;
    Ok(set != 0)
}

pub fn set_flag(conn: &Connection, file_id: FileId, flag: FileFlag, on: bool) -> Result<()> {
    conn.execute(
        "UPDATE files SET flags = CASE
             WHEN :on != 0 THEN flags | :bit
             ELSE flags & ~:bit
           END
         WHERE id = :id",
        named_params! {
            ":on": if on { 1i64 } else { 0i64 },
            ":bit": flag.mask_i64(),
            ":id": file_id.0,
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_round_trip_bits() {
        let mut flags = FileFlags::default();
        assert!(!flags.get(FileFlag::FileExtracted));
        flags.set(FileFlag::FileExtracted, true);
        flags.set(FileFlag::HasSparse, true);
        flags.set(FileFlag::CheckWithCanonicalCompleted, true);
        flags.set(FileFlag::ErrorWhileDedup, true);
        flags.set(FileFlag::AppendedPath, true);
        assert!(flags.get(FileFlag::FileExtracted));
        assert!(!flags.get(FileFlag::Modified));
        assert!(flags.get(FileFlag::HasSparse));
        assert!(flags.get(FileFlag::CheckWithCanonicalCompleted));
        assert!(flags.get(FileFlag::ErrorWhileDedup));
        assert!(flags.get(FileFlag::AppendedPath));
        assert_eq!(
            FileFlags::from_i64(flags.to_i64()).bits(),
            FileFlag::FileExtracted.mask()
                | FileFlag::HasSparse.mask()
                | FileFlag::CheckWithCanonicalCompleted.mask()
                | FileFlag::ErrorWhileDedup.mask()
                | FileFlag::AppendedPath.mask()
        );
    }
}
