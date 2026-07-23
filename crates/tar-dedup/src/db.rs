use std::path::Path;

use rusqlite::Connection;

use crate::config::{ExtractRuntimeState, RuntimeState};
use crate::db::flags::{FileFlag, FileFlags};
use crate::db::types::{
    ArchiveSession, FileId, FilePhase, GroupKey, NewFileRecord,
};
use crate::error::Result;

pub mod flags;
pub mod types;

mod archive;
mod common;
mod dedup;
mod extract;
mod filter;
mod hash;
mod inventory;
mod schema;
mod sparsify;
pub mod content_id;

pub use common::SqlFileRow;

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

    pub fn get_file<R: SqlFileRow>(&self, file_id: FileId) -> Result<Option<R>> {
        common::get_file(&self.conn, file_id)
    }
    // TODO: Resolution does happen to single file which is not deterministic.
    // pub fn get_file_by_tar_path(&self, tar_path: &str) -> Result<Option<FileRecord>> {
    //     inventory::get_file_by_tar_path(&self.conn, tar_path)
    // }

    // pub fn set_tar_path(&self, file_id: FileId, tar_path: &str) -> Result<()> {
    //     inventory::set_tar_path(&self.conn, file_id, tar_path)
    // }

    pub fn count_files(&self) -> Result<u64> {
        inventory::count_files(&self.conn)
    }

    pub fn count_files_in_phase(&self, phase: FilePhase) -> Result<u64> {
        inventory::count_files_in_phase(&self.conn, phase)
    }

    pub fn files_in_phase<R: SqlFileRow>(&self, phase: FilePhase) -> Result<Vec<R>> {
        inventory::list_files_in_phase(&self.conn, phase)
    }

    pub fn mark_file_phase(&self, file_id: FileId, phase: FilePhase) -> Result<()> {
        inventory::mark_phase(&self.conn, file_id, phase)
    }

    pub fn get_flags(&self, file_id: FileId) -> Result<FileFlags> {
        flags::get_flags(&self.conn, file_id)
    }

    pub fn set_flags(&self, file_id: FileId, value: FileFlags) -> Result<()> {
        flags::set_flags(&self.conn, file_id, value)
    }

    pub fn get_flag(&self, file_id: FileId, flag: FileFlag) -> Result<bool> {
        flags::get_flag(&self.conn, file_id, flag)
    }

    pub fn set_flag(&self, file_id: FileId, flag: FileFlag, on: bool) -> Result<()> {
        flags::set_flag(&self.conn, file_id, flag, on)
    }

    pub fn update_file_inspection(&self, file_id: FileId, digest: [u8; 20], sparse_count: u64) -> Result<()> {
        hash::update_file_inspection(&self.conn, file_id, digest, sparse_count)
    }

    pub fn pending_duplicate_groups(&self) -> Result<Vec<GroupKey>> {
        dedup::pending_duplicate_groups(&self.conn)
    }

    pub fn promote_hashed_to_filtered(&self) -> Result<u64> {
        filter::promote_hashed_to_filtered(&self.conn)
    }

    pub fn promote_non_file_filtered_to_deduped(&self) -> Result<u64> {
        dedup::promote_non_file_filtered_to_deduped(&self.conn)
    }

    pub fn promote_null_sha1_filtered_to_deduped(&self) -> Result<u64> {
        dedup::promote_null_sha1_filtered_to_deduped(&self.conn)
    }

    pub fn promote_singleton_filtered_to_deduped(&self) -> Result<u64> {
        dedup::promote_singleton_filtered_to_deduped(&self.conn)
    }

    pub fn promote_deduped_to_sparsified(&self) -> Result<u64> {
        sparsify::promote_deduped_to_sparsified(&self.conn)
    }

    pub fn mark_active_canonical(&self, file_id: FileId) -> Result<()> {
        dedup::mark_active_canonical(&self.conn, file_id)
    }

    pub fn promote_to_deduped(&self, file_id: FileId) -> Result<()> {
        dedup::promote_to_deduped(&self.conn, file_id)
    }

    pub fn clear_check_with_canonical_completed(
        &self,
        sha1: &[u8; 20],
        size: u64,
    ) -> Result<()> {
        dedup::clear_check_with_canonical_completed(&self.conn, sha1, size)
    }

    pub fn promote_errored_pending_to_deduped(
        &self,
        sha1: &[u8; 20],
        size: u64,
    ) -> Result<u64> {
        dedup::promote_errored_pending_to_deduped(&self.conn, sha1, size)
    }

    pub fn count_check_with_canonical_completed(&self) -> Result<u64> {
        dedup::count_check_with_canonical_completed(&self.conn)
    }

    pub fn count_active_canonicals(&self, sha1: &[u8; 20], size: u64) -> Result<u64> {
        dedup::count_active_canonicals(&self.conn, sha1, size)
    }

    pub fn promote_active_canonical_in_group(&self, sha1: &[u8; 20], size: u64) {
        dedup::promote_active_canonical_in_group(&self.conn, sha1, size)
    }

    pub fn count_electable_pending(&self, sha1: &[u8; 20], size: u64) -> Result<u64> {
        dedup::count_electable_pending(&self.conn, sha1, size)
    }

    pub fn list_filtered_in_group<R: SqlFileRow>(
        &self,
        sha1: &[u8; 20],
        size: u64,
    ) -> Result<Vec<R>> {
        dedup::list_filtered_in_group(&self.conn, sha1, size)
    }

    // TODO: Mark file and descendants in Phase
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

    pub fn install_initial_manifest(snapshot_path: &Path, db_path: &Path) -> Result<()> {
        extract::install_initial_manifest(snapshot_path, db_path)
    }

    pub fn apply_snapshot_archived_flags(&self, snapshot_path: &Path) -> Result<u64> {
        extract::apply_snapshot_archived_flags(&self.conn, snapshot_path)
    }

    // pub fn promote_cached_tar_member(&self, tar_path: &str) -> Result<()> {
    //     extract::promote_cached_tar_member(&self.conn, tar_path)
    // }

    pub fn count_unconfirmed_restored(&self) -> Result<u64> {
        extract::count_unconfirmed_restored(&self.conn)
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

    pub fn list_files_to_restore<R: SqlFileRow>(&self) -> Result<Vec<R>> {
        extract::list_files_to_restore(&self.conn)
    }

    // pub fn tar_member_path(&self, record: &FileRecord) -> Result<String> {
    //     extract::tar_member_path(&self.conn, record)
    // }

    pub fn init_extract_runtime_state(&self) -> Result<()> {
        extract::init_extract_runtime_state(&self.conn)
    }
}
