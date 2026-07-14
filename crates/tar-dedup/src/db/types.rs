use chrono::{DateTime, Utc};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileId(pub i64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentId(pub String);

#[derive(Debug, Clone)]
pub struct FileRecord {
    pub id: FileId,
    pub rel_path: PathBuf,
    pub size: u64,
    pub sha1: Option<[u8; 20]>,
    #[allow(dead_code)]
    pub mtime: Option<DateTime<Utc>>,
    #[allow(dead_code)]
    pub atime: Option<DateTime<Utc>>,
    #[allow(dead_code)]
    pub uid: Option<u32>,
    #[allow(dead_code)]
    pub gid: Option<u32>,
    #[allow(dead_code)]
    pub mode: Option<u32>,
    #[allow(dead_code)]
    pub canonical_id: Option<FileId>,
    /// Staged/tar member name (`content_id`); set for canonical files at stage time.
    pub tar_path: Option<String>,
    /// Set when an ingested snapshot lists this row (or its canonical) as `archived`.
    pub snapshot_archived: bool,
}

#[derive(Debug, Clone)]
pub struct NewFileRecord {
    pub rel_path: PathBuf,
    pub size: u64,
    pub mtime: Option<DateTime<Utc>>,
    pub atime: Option<DateTime<Utc>>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: Option<u32>,
}

/// Enum represents all possible targets a symlink can have. `Unknown` is for dangling links that
/// could not be resolved. Destination is to be understood as the transitive closure of any given
/// length of symlinks i.e. A -> B -> C -> D Will lead to A,B,C all having the same link type even
/// though only C points to LinkType and A,B point to symlinks themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkType {
    File,
    Directory,
    Socket,
    FIFO,
    CharacterDevice,
    BlockDevice,
    Dangling,
    Cycle,
    /// Emitted if an error is encountered while accessing the file type.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Directory,
    Socket,
    FIFO,
    CharacterDevice,
    BlockDevice,
    Symlink(LinkType),
    /// Emitted if an error is encountered while accessing the file type.
    Unknown,
}

impl LinkType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "dir",
            Self::FIFO => "fifo",
            Self::CharacterDevice => "char_dev",
            Self::BlockDevice => "block_dev",
            Self::Dangling => "dangling",
            Self::Cycle => "cycle",
            Self::Socket => "socket",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(raw: &str) -> crate::error::Result<Self> {
        match raw {
            "file" => Ok(Self::File),
            "dir" => Ok(Self::Directory),
            "fifo" => Ok(Self::FIFO),
            "char_dev" => Ok(Self::CharacterDevice),
            "block_dev" => Ok(Self::BlockDevice),
            "dangling" => Ok(Self::Dangling),
            "cycle" => Ok(Self::Cycle),
            "socket" => Ok(Self::Socket),
            "unknown" => Ok(Self::Unknown),
            other => Err(crate::error::Error::Other(anyhow::anyhow!(
                "unknown LinkType: {other}"
            ))),
        }
    }
}

impl FileType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "dir",
            Self::FIFO => "fifo",
            Self::CharacterDevice => "char_dev",
            Self::BlockDevice => "block_dev",
            Self::Socket => "socket",
            Self::Unknown => "unknown",
            Self::Symlink(LinkType::File) => "symlink_file",
            Self::Symlink(LinkType::Directory) => "symlink_dir",
            Self::Symlink(LinkType::Socket) => "symlink_socket",
            Self::Symlink(LinkType::FIFO) => "symlink_fifo",
            Self::Symlink(LinkType::CharacterDevice) => "symlink_char_dev",
            Self::Symlink(LinkType::BlockDevice) => "symlink_block_dev",
            Self::Symlink(LinkType::Dangling) => "symlink_dangling",
            Self::Symlink(LinkType::Cycle) => "symlink_cycle",
            Self::Symlink(LinkType::Unknown) => "symlink_unknown",
        }
    }

    pub fn parse(raw: &str) -> crate::error::Result<Self> {
        match raw {
            "file" => Ok(Self::File),
            "dir" => Ok(Self::Directory),
            "fifo" => Ok(Self::FIFO),
            "char_dev" => Ok(Self::CharacterDevice),
            "block_dev" => Ok(Self::BlockDevice),
            "socket" => Ok(Self::Socket),
            "unknown" => Ok(Self::Unknown),
            // All Symlink variants.
            "symlink_file" => Ok(Self::Symlink(LinkType::File)),
            "symlink_dir" => Ok(Self::Symlink(LinkType::Directory)),
            "symlink_socket" => Ok(Self::Symlink(LinkType::Socket)),
            "symlink_fifo" => Ok(Self::Symlink(LinkType::FIFO)),
            "symlink_char_dev" => Ok(Self::Symlink(LinkType::CharacterDevice)),
            "symlink_block_dev" => Ok(Self::Symlink(LinkType::BlockDevice)),
            "symlink_dangling" => Ok(Self::Symlink(LinkType::Dangling)),
            "symlink_cycle" => Ok(Self::Symlink(LinkType::Cycle)),
            "symlink_unknown" => Ok(Self::Symlink(LinkType::Unknown)),
            other => Err(crate::error::Error::Other(anyhow::anyhow!(
                "unknown FileType: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilePhase {
    // Archive pipeline
    Inventoried,
    Hashed,
    Deduped,
    Staged,
    Archived,
    // Extract pipeline — placement at final rel_path
    /// Ready to restore (payload may already be in extract cache).
    Unarchived,
    /// Regular file restored at its final rel_path.
    AtDestination,
    /// Symlink (or link) restored at its final rel_path.
    LinkAtDestination,
}

impl FilePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inventoried => "inventoried",
            Self::Hashed => "hashed",
            Self::Deduped => "deduped",
            Self::Staged => "staged",
            Self::Archived => "archived",
            Self::Unarchived => "unarchived",
            Self::AtDestination => "at_destination",
            Self::LinkAtDestination => "link_at_destination",
        }
    }

    pub fn parse(raw: &str) -> crate::error::Result<Self> {
        match raw {
            "inventoried" => Ok(Self::Inventoried),
            "hashed" => Ok(Self::Hashed),
            "deduped" => Ok(Self::Deduped),
            "staged" => Ok(Self::Staged),
            "archived" => Ok(Self::Archived),
            "unarchived" => Ok(Self::Unarchived),
            "at_destination" => Ok(Self::AtDestination),
            "link_at_destination" => Ok(Self::LinkAtDestination),
            other => Err(crate::error::Error::Config(format!(
                "unknown file phase: {other}"
            ))),
        }
    }

    pub fn is_archive_phase(self) -> bool {
        matches!(
            self,
            Self::Inventoried
                | Self::Hashed
                | Self::Deduped
                | Self::Staged
                | Self::Archived
        )
    }

    pub fn is_extract_phase(self) -> bool {
        !self.is_archive_phase()
    }
}

#[derive(Debug, Clone)]
pub struct DuplicateGroup {
    pub members: Vec<FileId>,
}

#[derive(Debug, Clone)]
pub struct ArchiveSession {
    pub id: i64,
    pub archive_offset: u64,
}

//==================================================================================================
// Testing
//==================================================================================================

#[test]
fn link_type_as_str_roundtrip() {
    let kinds = [
        LinkType::File,
        LinkType::Directory,
        LinkType::Socket,
        LinkType::FIFO,
        LinkType::CharacterDevice,
        LinkType::BlockDevice,
        LinkType::Dangling,
        LinkType::Cycle,
        LinkType::Unknown,
    ];

    for kind in kinds {
        assert_eq!(LinkType::parse(kind.as_str()).expect("parse"), kind);
    }
}

#[test]
fn file_type_as_str_roundtrip() {
    let kinds = [
        FileType::File,
        FileType::Directory,
        FileType::Socket,
        FileType::FIFO,
        FileType::CharacterDevice,
        FileType::BlockDevice,
        FileType::Unknown,
        FileType::Symlink(LinkType::File),
        FileType::Symlink(LinkType::Directory),
        FileType::Symlink(LinkType::Socket),
        FileType::Symlink(LinkType::FIFO),
        FileType::Symlink(LinkType::CharacterDevice),
        FileType::Symlink(LinkType::BlockDevice),
        FileType::Symlink(LinkType::Dangling),
        FileType::Symlink(LinkType::Cycle),
        FileType::Symlink(LinkType::Unknown),
    ];

    for kind in kinds {
        assert_eq!(FileType::parse(kind.as_str()).expect("parse"), kind);
    }
}
