use rusqlite::{named_params, Connection};

use crate::db::types::FileId;
use crate::error::Result;

/// Bit index into [`FileFlags`] (not the mask itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u64)]
pub enum FileFlag {
    /// An IO Error occurred while querying the metadata of this file
    IoErrorWhileInventorying = 0,
    /// This row is the hardlink group’s canonical payload (same `(dev, inode)`).
    /// The flag is only applied iff there is more than one row with matching `(dev, inode)`
    FileHardlinkCanonical = 1,
    /// An Error occurred while querying the xattrs of this file
    XAttrError = 2,
    /// An Error occurred while querying the acls of this file.
    PosixAclError = 3,
    /// An Error occurred while querying SELinux Policies
    SELinuxError = 4,
    /// File changed while the archive pipeline was touching it.
    Modified = 5,
    /// An error occurred during the first scan pass (sha + hole)
    ErrorWhileHash = 6,
    /// Compare vs this round's canonical finished; content differs.
    /// Cleared on round end for the whole `(sha1, size)` group.
    CheckWithCanonicalCompleted = 7,
    /// Read/compare failed during dedup. Sticky — never cleared on later success.
    /// Excludes the file from canonical election.
    ErrorWhileDedup = 8,
    /// Sparse rewrite exists; stage/archive should use the sparsified target.
    HasSparse = 9,
    /// Sparse copy failed (permissions, IO, …). Sticky.
    ErrorWhileSparsify = 10,
    /// Payload was written into an archive session.
    /// Set on successful `append_path`. Left standing when the session finalizes
    /// (`phase` → `archived`). Cleared only on abort/truncate for rows that are
    /// still not `archived` (incomplete session rewrite).
    AppendedPath = 11,
    /// `append_path` failed during the archive process.
    ErrorWhileArchive = 12,

    /// Payload for this content landed in the extract cache (canonical row only).
    /// Set after a successful `unpack`; cleared on catalog install normalization.
    FileExtracted = 13,
    /// An error occurred while extracting a given file
    ErrorWhileExtracting = 14,
    /// Rehashing Encountered a mismatch
    RehashMismatch = 15,
    /// While attempting the rehash, an error occurred
    ErrorWhileRehashing = 16,
    /// Source ready for linking
    AtLinkSource = 17,
    /// An Error prevented the file from being placed in its correct position
    ErrorWhilePlacing = 18,
    /// At least one error occurred while applying metadata
    ErrorWhileApplyingMetadata = 19,
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

/// Bit index into [`SourceFlags`] (not the mask itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u64)]
pub enum SourceFlag {
    /// This source row is a directory tree root (`-i` or a directory `--files-from` line).
    IsDirectory = 0,
}

impl SourceFlag {
    pub const fn mask(self) -> u64 {
        1u64 << (self as u8)
    }

    pub const fn mask_i64(self) -> i64 {
        self.mask() as i64
    }
}

/// Bitset stored in `source.flags`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct SourceFlags(u64);

impl SourceFlags {
    pub const fn from_bits(bits: u64) -> Self {
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

    pub fn get(self, flag: SourceFlag) -> bool {
        self.0 & flag.mask() != 0
    }

    pub fn set(&mut self, flag: SourceFlag, on: bool) {
        if on {
            self.0 |= flag.mask();
        } else {
            self.0 &= !flag.mask();
        }
    }

    pub fn with(mut self, flag: SourceFlag, on: bool) -> Self {
        self.set(flag, on);
        self
    }
}

/// Bit index into [`OutTreeFlags`] (not the mask itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u64)]
pub enum OutTreeFlag {
    /// Output path was walked / stat'd during place (resume skip).
    AlreadyWalked = 0,
    /// Payload was materialized at this output path.
    Extracted = 1,
    /// Copy/link into this output path failed.
    ErrorWhilePlace = 2,
    /// Metadata restore failed for this output path.
    ErrorWhileApplyingMetadata = 3,
    /// Highlight directories to be able to scan them for dir tree creation.
    IsDirectory = 5,
}

impl OutTreeFlag {
    pub const fn mask(self) -> u64 {
        1u64 << (self as u8)
    }

    pub const fn mask_i64(self) -> i64 {
        self.mask() as i64
    }
}

/// Bitset stored in `out_tree.flags`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct OutTreeFlags(u64);

impl OutTreeFlags {
    pub const fn from_bits(bits: u64) -> Self {
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

    pub fn get(self, flag: OutTreeFlag) -> bool {
        self.0 & flag.mask() != 0
    }

    pub fn set(&mut self, flag: OutTreeFlag, on: bool) {
        if on {
            self.0 |= flag.mask();
        } else {
            self.0 &= !flag.mask();
        }
    }

    pub fn with(mut self, flag: OutTreeFlag, on: bool) -> Self {
        self.set(flag, on);
        self
    }
}

pub fn insert_ref(
    conn: &Connection,
    source_id: i64,
    file_id: FileId,
) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO ref (source_id, file_id)
         VALUES (:source_id, :file_id)",
        named_params! {
            ":source_id": source_id,
            ":file_id": file_id.0,
        },
    )?;
    Ok(n > 0)
}

pub fn get_file_flags(conn: &Connection, file_id: FileId) -> Result<FileFlags> {
    let raw: i64 = conn.query_row(
        "SELECT flags FROM files WHERE id = :id",
        named_params! { ":id": file_id.0 },
        |row| row.get(0),
    )?;
    Ok(FileFlags::from_i64(raw))
}

pub fn set_file_flags(conn: &Connection, file_id: FileId, flags: FileFlags) -> Result<()> {
    conn.execute(
        "UPDATE files SET flags = :flags WHERE id = :id",
        named_params! {
            ":flags": flags.to_i64(),
            ":id": file_id.0,
        },
    )?;
    Ok(())
}

pub fn get_file_flag(conn: &Connection, file_id: FileId, flag: FileFlag) -> Result<bool> {
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

pub fn set_file_flag(conn: &Connection, file_id: FileId, flag: FileFlag, on: bool) -> Result<u64> {
    let rows_affected = conn.execute(
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
    Ok(rows_affected as u64)
}

pub fn get_out_tree_flags(conn: &Connection, out_id: crate::db::types::OutTreeId) -> Result<OutTreeFlags> {
    let raw: i64 = conn.query_row(
        "SELECT flags FROM out_tree WHERE id = :id",
        named_params! { ":id": out_id.0 },
        |row| row.get(0),
    )?;
    Ok(OutTreeFlags::from_i64(raw))
}

pub fn set_out_tree_flags(
    conn: &Connection,
    out_id: crate::db::types::OutTreeId,
    flags: OutTreeFlags,
) -> Result<()> {
    conn.execute(
        "UPDATE out_tree SET flags = :flags WHERE id = :id",
        named_params! {
            ":flags": flags.to_i64(),
            ":id": out_id.0,
        },
    )?;
    Ok(())
}

pub fn get_out_tree_flag(
    conn: &Connection,
    out_id: crate::db::types::OutTreeId,
    flag: OutTreeFlag,
) -> Result<bool> {
    let set: i64 = conn.query_row(
        "SELECT (flags & :bit) != 0 FROM out_tree WHERE id = :id",
        named_params! {
            ":bit": flag.mask_i64(),
            ":id": out_id.0,
        },
        |row| row.get(0),
    )?;
    Ok(set != 0)
}

pub fn set_out_tree_flag(
    conn: &Connection,
    out_id: crate::db::types::OutTreeId,
    flag: OutTreeFlag,
    on: bool,
) -> Result<u64> {
    let rows_affected = conn.execute(
        "UPDATE out_tree SET flags = CASE
             WHEN :on != 0 THEN flags | :bit
             ELSE flags & ~:bit
           END
         WHERE id = :id",
        named_params! {
            ":on": if on { 1i64 } else { 0i64 },
            ":bit": flag.mask_i64(),
            ":id": out_id.0,
        },
    )?;
    Ok(rows_affected as u64)
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

    #[test]
    fn source_flag_round_trip_bits() {
        let mut flags = SourceFlags::default();
        assert!(!flags.get(SourceFlag::IsDirectory));
        flags.set(SourceFlag::IsDirectory, true);
        assert!(flags.get(SourceFlag::IsDirectory));
        assert_eq!(
            SourceFlags::from_i64(flags.to_i64()).bits(),
            SourceFlag::IsDirectory.mask()
        );
    }

    #[test]
    fn out_tree_flag_round_trip_bits() {
        let mut flags = OutTreeFlags::default();
        assert!(!flags.get(OutTreeFlag::AlreadyWalked));
        flags.set(OutTreeFlag::AlreadyWalked, true);
        flags.set(OutTreeFlag::Extracted, true);
        assert!(flags.get(OutTreeFlag::AlreadyWalked));
        assert!(flags.get(OutTreeFlag::Extracted));
        assert_eq!(
            OutTreeFlags::from_i64(flags.to_i64()).bits(),
            OutTreeFlag::AlreadyWalked.mask() | OutTreeFlag::Extracted.mask()
        );
    }
}
