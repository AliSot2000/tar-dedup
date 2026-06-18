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
    Inventoried,
    Hashed,
    #[allow(dead_code)]
    Deduped,
    Staged,
    Archived,
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
