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

mod tar_writer;
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

    pub fn resolve_numeric_ids(&self) -> Result<()> {
        inventory::resolve_numeric_ids(&self.conn)
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

    pub fn promote_non_sparsify_candidates_to_sparsified(&self, min_pages: u64) -> Result<u64> {
        sparsify::promote_non_sparsify_candidates_to_sparsified(&self.conn, min_pages)
    }

    pub fn list_sparsify_candidates<R: SqlFileRow>(&self, min_pages: u64) -> Result<Vec<R>> {
        sparsify::list_sparsify_candidates(&self.conn, min_pages)
    }

    pub fn mark_sparsified_sparse(&self, file_id: FileId) -> Result<()> {
        sparsify::mark_sparsified_sparse(&self.conn, file_id)
    }

    pub fn mark_sparsified_error(&self, file_id: FileId) -> Result<()> {
        sparsify::mark_sparsified_error(&self.conn, file_id)
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
        tar_writer::begin_session(&self.conn, archive_offset)
    }

    pub fn stamp_archive_session_finished_at(&self, session_id: i64) -> Result<()> {
        tar_writer::stamp_session_finished_at(&self.conn, session_id)
    }

    pub fn finalize_archive_session(&self, session_id: i64) -> Result<()> {
        tar_writer::finalize_session(&self.conn, session_id)
    }

    pub fn mark_archive_session_aborted(&self, session_id: i64) -> Result<()> {
        tar_writer::mark_session_aborted(&self.conn, session_id)
    }

    pub fn abort_incomplete_archive_session(
        &self,
        session: &ArchiveSession,
    ) -> Result<()> {
        tar_writer::abort_incomplete_session(&self.conn, session)
    }

    pub fn promote_pending_archived(&self) -> Result<u64> {
        tar_writer::promote_pending_archived(&self.conn)
    }

    pub fn mark_archive_session_pending(&self, file_id: FileId) -> Result<()> {
        tar_writer::mark_archive_session_pending(&self.conn, file_id)
    }

    pub fn clear_archive_session_pending(&self) -> Result<u64> {
        tar_writer::clear_archive_session_pending(&self.conn)
    }

    pub fn open_archive_session(&self) -> Result<Option<ArchiveSession>> {
        tar_writer::open_session(&self.conn)
    }

    pub fn has_finalized_archive_session(&self) -> Result<bool> {
        tar_writer::has_finalized_session(&self.conn)
    }

    pub fn reset_archive_state(&self) -> Result<()> {
        tar_writer::reset_archive_state(&self.conn)
    }

    pub fn sum_canonical_bytes_to_archive(&self, filter_sha: bool) -> Result<u64> {
        tar_writer::sum_canonical_bytes_to_archive(&self.conn, filter_sha)
    }

    pub fn sum_archived_canonical_bytes(&self, filter_sha: bool) -> Result<u64> {
        tar_writer::sum_archived_canonical_bytes(&self.conn, filter_sha)
    }

    /// Staged canonical ids ordered by extension / size / id for the archive pass.
    pub fn list_staged_canonical_ordered(&self, filter_sha: bool) -> Result<Vec<FileId>> {
        tar_writer::list_staged_canonical_ordered(&self.conn, filter_sha)
    }

    pub fn get_archive_bytes_in(&self) -> Result<u64> {
        tar_writer::get_archive_bytes_in(&self.conn)
    }

    pub fn get_archive_bytes_out(&self) -> Result<Option<u64>> {
        tar_writer::get_archive_bytes_out(&self.conn)
    }

    pub fn set_archive_bytes_in(&self, value: u64) -> Result<()> {
        tar_writer::set_archive_bytes_in(&self.conn, value)
    }

    pub fn set_archive_bytes_out(&self, value: u64) -> Result<()> {
        tar_writer::set_archive_bytes_out(&self.conn, value)
    }

    pub fn promote_ineligible_to_archived(&self, filter_sha: bool) -> Result<u64> {
        tar_writer::promote_ineligible_to_archived(&self.conn, filter_sha)
    }

    pub fn promote_remainder_to_archived(&self) -> Result<u64> {
        tar_writer::promote_remainder_to_archived(&self.conn)
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

    pub fn init_extract_runtime_state(&self) -> Result<()> {
        extract::init_extract_runtime_state(&self.conn)
    }
}
