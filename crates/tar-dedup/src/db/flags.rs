use rusqlite::{named_params, Connection};

use crate::db::types::FileId;
use crate::error::Result;

/// Single bit position inside a [`FlagSet`] (the discriminant, not the mask).
///
/// Discriminants must be in `0..=62` ([`MAX_FLAG_BIT`]).
pub trait FlagBit: Copy {
    fn bit_index(self) -> u8;

    fn mask(self) -> u64 {
        1u64 << self.bit_index()
    }

    fn mask_i64(self) -> i64 {
        self.mask() as i64
    }
}

/// Bitset stored as SQLite `INTEGER` (sign bit cleared on read/write).
pub trait FlagSet: Copy + Default + PartialEq + Eq {
    type Flag: FlagBit;

    fn bits(self) -> u64;
    fn from_bits(bits: u64) -> Self;
    fn set_bits(&mut self, bits: u64);

    fn from_i64(raw: i64) -> Self {
        Self::from_bits(raw as u64)
    }

    fn to_i64(self) -> i64 {
        self.bits() as i64
    }

    fn get(self, flag: Self::Flag) -> bool {
        self.bits() & flag.mask() != 0
    }

    fn set(&mut self, flag: Self::Flag, on: bool) {
        let bits = self.bits();
        self.set_bits(if on {
            bits | flag.mask()
        } else {
            bits & !flag.mask()
        });
    }

    fn with(mut self, flag: Self::Flag, on: bool) -> Self {
        self.set(flag, on);
        self
    }
}

/// Highest allowed [`FlagBit`] index. Bit 63 is reserved (SQLite `INTEGER` sign bit).
pub const MAX_FLAG_BIT: u8 = 62;

const SIGN_BIT: u64 = 1u64 << 63;

const fn assert_flag_bit(bit: u64) {
    if bit > MAX_FLAG_BIT as u64 {
        panic!("flag bit index must be 0..=62 (bit 63 reserved for SQLite INTEGER sign)");
    }
}

macro_rules! define_flags {
    (
        $(#[$enum_meta:meta])*
        $enum_vis:vis enum $Flag:ident {
            $($(#[$variant_meta:meta])* $variant:ident = $bit:literal),* $(,)?
        }
        $(#[$set_meta:meta])*
        $set_vis:vis struct $Flags:ident;
    ) => {
        $( const _: () = { assert_flag_bit($bit as u64); }; )*

        $(#[$enum_meta])*
        $enum_vis enum $Flag {
            $($(#[$variant_meta])* $variant = $bit,)*
        }

        impl FlagBit for $Flag {
            #[inline]
            fn bit_index(self) -> u8 {
                self as u8
            }
        }

        impl $Flag {
            #[inline]
            pub fn mask(self) -> u64 {
                <Self as FlagBit>::mask(self)
            }

            #[inline]
            pub fn mask_i64(self) -> i64 {
                <Self as FlagBit>::mask_i64(self)
            }
        }

        $(#[$set_meta])*
        $set_vis struct $Flags(u64);

        impl $Flags {
            #[inline]
            pub const fn from_bits(bits: u64) -> Self {
                Self(bits & !SIGN_BIT)
            }

            #[inline]
            pub const fn bits(self) -> u64 {
                self.0
            }

            #[inline]
            pub fn from_i64(raw: i64) -> Self {
                <Self as FlagSet>::from_i64(raw)
            }

            #[inline]
            pub fn to_i64(self) -> i64 {
                <Self as FlagSet>::to_i64(self)
            }

            #[inline]
            pub fn get(self, flag: $Flag) -> bool {
                <Self as FlagSet>::get(self, flag)
            }

            #[inline]
            pub fn set(&mut self, flag: $Flag, on: bool) {
                <Self as FlagSet>::set(self, flag, on)
            }

            #[inline]
            pub fn with(self, flag: $Flag, on: bool) -> Self {
                <Self as FlagSet>::with(self, flag, on)
            }
        }

        impl FlagSet for $Flags {
            type Flag = $Flag;

            #[inline]
            fn bits(self) -> u64 {
                self.0
            }

            #[inline]
            fn from_bits(bits: u64) -> Self {
                Self::from_bits(bits)
            }

            #[inline]
            fn set_bits(&mut self, bits: u64) {
                self.0 = bits & !SIGN_BIT;
            }
        }

        impl Default for $Flags {
            fn default() -> Self {
                Self::from_bits(0)
            }
        }
    };
}

define_flags! {
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
        /// Used RefLink (if false -> used (sparse) copy)
        UsedRefLink = 18,
        /// An Error prevented the file from being placed in its correct position
        ErrorWhilePlacing = 19,
        /// At least one error occurred while applying metadata
        ErrorWhileApplyingMetadata = 20,
    }
    /// Bitset stored in `files.flags`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FileFlags;
}

define_flags! {
    /// Bit index into [`SourceFlags`] (not the mask itself).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    #[repr(u64)]
    pub enum SourceFlag {
        /// This source row is a directory tree root (`-i` or a directory `--files-from` line).
        IsDirectory = 0,
    }
    /// Bitset stored in `source.flags`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct SourceFlags;
}

define_flags! {
    /// Bit index into [`OutTreeFlags`] (not the mask itself).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    #[repr(u64)]
    pub enum OutTreeFlag {
        /// Payload was materialized at this output path or linked in case of link tree.
        Placed = 0,
        /// Payload was hard-linked to existing hard link group
        IsHardlink = 1,
        /// Marks this file as the canonical (hard link target)
        IsCanonical = 2,
        /// Walked this abs_path already with another source. Do not hard link against these entries.
        EntryWalked = 3,
        /// Copy/link into this output path failed.
        ErrorWhilePlace = 4,
        /// Metadata restore failed for this output path.
        ErrorWhileApplyingMetadata = 5,
        /// Highlight directories to be able to scan them for dir tree creation.
        IsDirectory = 6,
    }
    /// Bitset stored in `out_tree.flags`.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct OutTreeFlags;
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
        assert!(!flags.get(OutTreeFlag::Placed));
        flags.set(OutTreeFlag::Placed, true);
        flags.set(OutTreeFlag::IsDirectory, true);
        assert!(flags.get(OutTreeFlag::Placed));
        assert!(flags.get(OutTreeFlag::IsDirectory));
        assert_eq!(
            OutTreeFlags::from_i64(flags.to_i64()).bits(),
            OutTreeFlag::Placed.mask() | OutTreeFlag::IsDirectory.mask()
        );
    }

    #[test]
    fn max_file_flag_bit_within_range() {
        assert!(FileFlag::ErrorWhileApplyingMetadata as u8 <= MAX_FLAG_BIT);
    }
}
