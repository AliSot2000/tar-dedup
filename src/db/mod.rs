use std::path::Path;

use rusqlite::Connection;

use crate::config::{ExtractRuntimeState, RuntimeState};
use crate::db::types::{
    ArchiveSession, DuplicateGroup, FileId, FilePhase, FileRecord, NewFileRecord,
};
use crate::error::Result;

pub mod types;

mod archive;
mod dedup;
mod extract;
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

    pub fn insert_file(&self, record: &NewFileRecord) -> Result<bool> {
        inventory::insert_file(&self.conn, record)
    }

    pub fn get_file(&self, file_id: FileId) -> Result<Option<FileRecord>> {
        inventory::get_file(&self.conn, file_id)
    }

    pub fn get_file_by_tar_path(&self, tar_path: &str) -> Result<Option<FileRecord>> {
        inventory::get_file_by_tar_path(&self.conn, tar_path)
    }

    pub fn set_tar_path(&self, file_id: FileId, tar_path: &str) -> Result<()> {
        inventory::set_tar_path(&self.conn, file_id, tar_path)
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

    pub fn set_canonical(&self, file_id: FileId, canonical_id: FileId) -> Result<()> {
        dedup::set_canonical(&self.conn, file_id, canonical_id)
    }

    pub fn mark_self_canonical(&self, file_id: FileId) -> Result<()> {
        dedup::mark_self_canonical(&self.conn, file_id)
    }

    pub fn list_canonical_files(&self, phase: FilePhase) -> Result<Vec<FileId>> {
        dedup::list_canonical_files(&self.conn, phase)
    }

    pub fn load_runtime_state(&self) -> Result<Option<RuntimeState>> {
        inventory::load_runtime_state(&self.conn)
    }

    pub fn save_runtime_state(&self, state: &RuntimeState) -> Result<()> {
        inventory::save_runtime_state(&self.conn, state)
    }

    pub fn begin_archive_session(&self, archive_offset: u64) -> Result<i64> {
        let stream_index = archive::next_stream_index(&self.conn)?;
        archive::begin_session(&self.conn, stream_index, archive_offset)
    }

    pub fn finalize_archive_session(&self, session_id: i64, bytes_in: u64, bytes_out: u64) -> Result<()> {
        archive::finalize_session(&self.conn, session_id, bytes_in, bytes_out)
    }

    pub fn open_archive_session(&self) -> Result<Option<ArchiveSession>> {
        archive::open_session(&self.conn)
    }

    pub fn reset_archive_state(&self) -> Result<()> {
        archive::reset_archive_state(&self.conn)
    }

    pub fn sum_canonical_bytes_to_archive(&self) -> Result<u64> {
        archive::sum_canonical_bytes_to_archive(&self.conn)
    }

    pub fn sum_archived_canonical_bytes(&self) -> Result<u64> {
        archive::sum_archived_canonical_bytes(&self.conn)
    }

    pub fn checkpoint(&self) -> Result<()> {
        self.conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    // --- Extract pipeline ---

    pub fn record_tar_seen(&self, tar_path: &str, size: u64) -> Result<Option<FileId>> {
        extract::record_tar_seen(&self.conn, tar_path, size)
    }

    pub fn prepare_materialize_restore(&self) -> Result<u64> {
        extract::prepare_materialize_restore(&self.conn)
    }

    pub fn load_extract_runtime_state(&self) -> Result<Option<ExtractRuntimeState>> {
        extract::load_extract_runtime_state(&self.conn)
    }

    pub fn save_extract_runtime_state(&self, state: &ExtractRuntimeState) -> Result<()> {
        extract::save_extract_runtime_state(&self.conn, state)
    }

    pub fn record_snapshot_ingested(&self) -> Result<u32> {
        extract::record_snapshot_ingested(&self.conn)
    }

    pub fn list_files_to_restore(&self) -> Result<Vec<FileRecord>> {
        extract::list_files_to_restore(&self.conn)
    }

    pub fn tar_member_path(&self, record: &FileRecord) -> Result<String> {
        extract::tar_member_path(&self.conn, record)
    }

    pub fn init_extract_runtime_state(&self) -> Result<()> {
        extract::init_extract_runtime_state(&self.conn)
    }
}
