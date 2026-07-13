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
    pub mtime: Option<i64>,
    #[allow(dead_code)]
    pub atime: Option<i64>,
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
    pub mtime: Option<i64>,
    pub atime: Option<i64>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: Option<u32>,
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
