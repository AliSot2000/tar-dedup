use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::config::PipelinePhase;

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
    pub mtime: Option<i64>,
    pub atime: Option<i64>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: Option<u32>,
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
    Deduped,
    Staged,
    Archived,
}

#[derive(Debug, Clone)]
pub struct DuplicateGroup {
    pub sha1: [u8; 20],
    pub size: u64,
    pub members: Vec<FileId>,
}

#[derive(Debug, Clone)]
pub struct ArchiveSession {
    pub id: i64,
    pub stream_index: i64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub finalized: bool,
}

#[derive(Debug, Clone)]
pub struct PipelineStatus {
    pub phase: PipelinePhase,
    pub snapshot_taken_at: DateTime<Utc>,
    pub max_workers: usize,
}
