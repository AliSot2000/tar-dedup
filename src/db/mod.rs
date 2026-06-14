use std::path::Path;

use rusqlite::Connection;

use crate::config::RuntimeState;
use crate::db::types::{
    ArchiveSession, DuplicateGroup, FileId, FilePhase, FileRecord, NewFileRecord, PipelineStatus,
};
use crate::error::Result;

pub mod types;

mod archive;
mod dedup;
mod hash;
mod inventory;
mod schema;

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::error::Error::io(parent, e))?;
        }
        let conn = Connection::open(path)?;
        schema::initialize(&conn)?;
        Ok(Self { conn })
    }

    pub fn insert_file(&self, record: &NewFileRecord) -> Result<FileId> {
        inventory::insert_file(&self.conn, record)
    }

    pub fn get_file(&self, file_id: FileId) -> Result<Option<FileRecord>> {
        inventory::get_file(&self.conn, file_id)
    }

    pub fn count_files(&self) -> Result<u64> {
        inventory::count_files(&self.conn)
    }

    pub fn files_in_phase(&self, phase: FilePhase) -> Result<Vec<FileRecord>> {
        inventory::list_files_in_phase(&self.conn, phase)
    }

    pub fn mark_file_phase(&self, file_id: FileId, phase: FilePhase) -> Result<()> {
        inventory::mark_phase(&self.conn, file_id, phase)
    }

    pub fn set_sha1(&self, file_id: FileId, digest: [u8; 20]) -> Result<()> {
        hash::set_sha1(&self.conn, file_id, digest)
    }

    pub fn duplicate_groups(&self) -> Result<Vec<DuplicateGroup>> {
        hash::duplicate_groups(&self.conn)
    }

    pub fn canonical_for(&self, sha1: [u8; 20], size: u64) -> Result<Option<FileId>> {
        hash::canonical_for(&self.conn, sha1, size)
    }

    pub fn set_canonical(&self, file_id: FileId, canonical_id: FileId) -> Result<()> {
        dedup::set_canonical(&self.conn, file_id, canonical_id)
    }

    pub fn mark_self_canonical(&self, file_id: FileId) -> Result<()> {
        dedup::mark_self_canonical(&self.conn, file_id)
    }

    pub fn list_canonical_files(&self) -> Result<Vec<FileId>> {
        dedup::list_canonical_files(&self.conn)
    }

    pub fn load_runtime_state(&self) -> Result<Option<RuntimeState>> {
        inventory::load_runtime_state(&self.conn)
    }

    pub fn save_runtime_state(&self, state: &RuntimeState) -> Result<()> {
        inventory::save_runtime_state(&self.conn, state)
    }

    pub fn pipeline_status(&self) -> Result<Option<PipelineStatus>> {
        Ok(self.load_runtime_state()?.map(|state| PipelineStatus {
            phase: state.phase,
            snapshot_taken_at: state.snapshot_taken_at,
            max_workers: state.max_workers,
        }))
    }

    pub fn begin_archive_session(&self) -> Result<i64> {
        let stream_index = archive::next_stream_index(&self.conn)?;
        archive::begin_session(&self.conn, stream_index)
    }

    pub fn finalize_archive_session(&self, session_id: i64, bytes_in: u64, bytes_out: u64) -> Result<()> {
        archive::finalize_session(&self.conn, session_id, bytes_in, bytes_out)
    }

    pub fn open_archive_session(&self) -> Result<Option<ArchiveSession>> {
        archive::open_session(&self.conn)
    }

    pub fn queue_archive_entry(&self, file_id: FileId, session_id: i64, tar_path: &str) -> Result<()> {
        archive::queue_entry(&self.conn, file_id, session_id, tar_path)
    }
}
