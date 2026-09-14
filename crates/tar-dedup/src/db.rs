use std::cell::{Ref, RefCell, RefMut};
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::config::{ExtractRuntimeState, RuntimeState};
use crate::db::flags::{FileFlag, FileFlags, OutTreeFlag, OutTreeFlags, SourceFlags};
use crate::db::types::{ArchiveSession, FileId, FilePhase, FilterExpression, GroupKey, NewFileRecord, NewOutTreeRow, OutTreeId, OutTreeRecord, SourceRecord, StrippedRecord};
use crate::error::Result;

pub mod flags;
pub mod types;

pub use errors::{ErrorPhase, ErrorRecord, RecordDraft};

mod tar_writer;
mod common;
mod dedup;
mod extract;
mod filter;
mod hash;
mod inventory;
pub mod meta;
mod schema;
mod sparsify;
pub mod content_id;
mod source;
mod stage;
mod scan;
mod rehash;
pub mod place;
mod permissions;
mod integrity;
mod errors;

pub use common::SqlFileRow;
pub use extract::ExtractScanState;
pub use meta::{dump_meta, MetaDump, MetaEntry, MetaKey};

pub struct Database {
    conn: RefCell<Connection>,
}

impl Database {
    fn conn(&self) -> Ref<'_, Connection> {
        self.conn.borrow()
    }

    fn conn_mut(&self) -> RefMut<'_, Connection> {
        self.conn.borrow_mut()
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| crate::error::Error::io(parent, e))?;
        }
        let conn = Connection::open(path)?;
        schema::initialize(&conn)?;
        Ok(Self {
            conn: RefCell::new(conn),
        })
    }

    pub fn abs_path_exists(&self, path: &Path) -> Result<bool> {
        inventory::abs_path_exists(&self.conn(), path)
    }

    pub fn file_id_by_abs_path(&self, path: &Path) -> Result<Option<FileId>> {
        inventory::file_id_by_abs_path(&*self.conn(), path)
    }

    pub fn add_ref(&self, source_id: i64, file_id: FileId) -> Result<bool> {
        flags::insert_ref(&*self.conn(), source_id, file_id)
    }

    pub fn insert_file(&self, record: &NewFileRecord) -> Result<bool> {
        inventory::insert_file(&*self.conn(), record)
    }

    /// Insert a new `files` row and a `ref` membership in one transaction.
    pub fn insert_file_and_ref(&self, source_id: i64, record: &NewFileRecord) -> Result<bool> {
        let mut conn = self.conn.borrow_mut();
        let tx = conn.transaction()?;
        let inserted = inventory::insert_file(&tx, record)?;
        let file_id = inventory::file_id_by_abs_path(&tx, &record.abs_path)?.ok_or_else(|| {
            crate::error::Error::Config(
                "insert_file_and_ref: file missing after insert".into(),
            )
        })?;
        flags::insert_ref(&tx, source_id, file_id)?;
        tx.commit()?;
        Ok(inserted)
    }

    pub fn add_get_source(&self, abs_path: &Path, source_kind: &str, line: Option<u64>, original_path: Option<&Path>, flags: SourceFlags) -> Result<i64> {
        source::add_get_source(&*self.conn(), abs_path, source_kind, line, original_path, flags)
    }

    pub fn find_overlapping_source(
        &self,
        abs_path: &Path,
        no_recursion: bool,
    ) -> Result<Option<(i64, PathBuf)>> {
        source::find_overlapping_source(&*self.conn(), abs_path, no_recursion)
    }

    pub fn list_sources(
        &self,
        only_dirs: Option<bool>,
        starting_id: i64,
        batch_size: u64,
    ) -> Result<Vec<SourceRecord>> {
        source::list_sources(&self.conn(), only_dirs, starting_id, batch_size)
    }

    pub fn get_file_by_id<R: SqlFileRow>(&self, file_id: FileId) -> Result<Option<R>> {
        common::get_file_by_id(&*self.conn(), file_id)
    }

    pub fn count_entries(&self) -> Result<u64> {
        common::count_entries(&*self.conn())
    }

    pub fn count_files_in_phase(&self, phase: FilePhase) -> Result<u64> {
        common::count_files_in_phase(&*self.conn(), phase)
    }

    pub fn files_in_phase<R: SqlFileRow>(&self, phase: FilePhase) -> Result<Vec<R>> {
        common::list_files_in_phase(&*self.conn(), phase)
    }

    pub fn mark_file_phase(&self, file_id: FileId, phase: FilePhase) -> Result<()> {
        common::mark_phase(&*self.conn(), file_id, phase)
    }

    pub fn resolve_numeric_ids(&self) -> Result<()> {
        inventory::resolve_numeric_ids(&*self.conn())
    }

    pub fn set_hardlink_canonicals(&self) -> Result<u64> {
        inventory::set_hardlink_canonicals(&*self.conn())
    }

    pub fn get_file_flags(&self, file_id: FileId) -> Result<FileFlags> {
        flags::get_file_flags(&*self.conn(), file_id)
    }

    pub fn set_file_flags(&self, file_id: FileId, value: FileFlags) -> Result<()> {
        flags::set_file_flags(&*self.conn(), file_id, value)
    }

    pub fn get_file_flag(&self, file_id: FileId, flag: FileFlag) -> Result<bool> {
        flags::get_file_flag(&*self.conn(), file_id, flag)
    }

    pub fn set_file_flag(&self, file_id: FileId, flag: FileFlag, on: bool) -> Result<u64> {
        flags::set_file_flag(&*self.conn(), file_id, flag, on)
    }

    pub fn get_out_tree_flags(&self, out_id: OutTreeId) -> Result<OutTreeFlags> {
        flags::get_out_tree_flags(&*self.conn(), out_id)
    }

    pub fn set_out_tree_flags(&self, out_id: OutTreeId, value: OutTreeFlags) -> Result<()> {
        flags::set_out_tree_flags(&*self.conn(), out_id, value)
    }

    pub fn get_out_tree_flag(&self, out_id: OutTreeId, flag: OutTreeFlag) -> Result<bool> {
        flags::get_out_tree_flag(&*self.conn(), out_id, flag)
    }

    pub fn set_out_tree_flag(&self, out_id: OutTreeId, flag: OutTreeFlag, on: bool) -> Result<u64> {
        flags::set_out_tree_flag(&*self.conn(), out_id, flag, on)
    }

    pub fn get_entries_to_hash<R: SqlFileRow>(&self, eager_filter: bool, detect_hardlinks: bool) -> Result<Vec<R>> {
        hash::get_entries_to_hash(&*self.conn(), eager_filter, detect_hardlinks)
    }

    pub fn count_all_hashable_files(&self, eager_filter: bool, detect_hardlinks: bool) -> Result<u64> {
        hash::count_all_hashable_files(&*self.conn(), eager_filter, detect_hardlinks)
    }

    pub fn update_file_inspection_per_id(&self, file_id: FileId, digest: [u8; 20], sparse_count: u64, update_hardlinks: bool) -> Result<()> {
        hash::update_file_inspection_per_id(&*self.conn(), file_id, digest, sparse_count, update_hardlinks)
    }

    pub fn pending_duplicate_groups(&self) -> Result<Vec<GroupKey>> {
        dedup::pending_duplicate_groups(&*self.conn())
    }

    pub fn add_include_pattern(&self, from: &str, line: Option<u64>, query: &str) -> Result<u64> {
        filter::add_include_pattern(&*self.conn(), from, line, query)
    }

    pub fn add_exclude_pattern(&self, from: &str, line: Option<u64>, query: &str) -> Result<u64> {
        filter::add_exclude_pattern(&*self.conn(), from, line, query)
    }

    pub fn count_filters(&self, exclude: Option<bool>) -> Result<u64> {
        filter::count_filters(&*self.conn(), exclude)
    }

    pub fn get_filters(&self, exclude: bool) -> Result<Vec<FilterExpression>> {
        filter::get_filters(&*self.conn(), exclude)
    }

    pub fn apply_no_filter(&self) -> Result<u64> {
        filter::apply_no_filter(&*self.conn())
    }

    pub fn get_rows_to_filter<R: SqlFileRow>(
        &self, last_id: Option<FileId>, eager_filter: bool, batch_size: u64)
        -> Result<Vec<R>> {
        filter::get_rows_to_filter(&*self.conn(), last_id, eager_filter, batch_size)
    }

    pub fn apply_filter_result<I: Iterator<Item = (FileId, i64, i64)>>(
        &self,
        results: I,
    ) -> Result<u64> {
        filter::apply_filter_result(&mut *self.conn_mut(), results)
    }

    pub fn fix_up_canonical_flag(&self) -> Result<(u64, u64)> {
        filter::fix_up_canonical_flag(&mut *self.conn_mut())
    }

    pub fn promote_non_file_filtered_to_deduped(&self) -> Result<u64> {
        dedup::promote_non_file_filtered_to_deduped(&*self.conn())
    }

    pub fn promote_null_sha1_filtered_to_deduped(&self) -> Result<u64> {
        dedup::promote_null_sha1_filtered_to_deduped(&*self.conn())
    }

    pub fn promote_singleton_filtered_to_deduped(&self) -> Result<u64> {
        dedup::promote_singleton_filtered_to_deduped(&*self.conn())
    }

    pub fn promote_deduped_to_sparsified(&self) -> Result<u64> {
        sparsify::promote_deduped_to_sparsified(&*self.conn())
    }

    pub fn promote_non_sparsify_candidates_to_sparsified(&self, min_pages: u64) -> Result<u64> {
        sparsify::promote_non_sparsify_candidates_to_sparsified(&*self.conn(), min_pages)
    }

    pub fn list_sparsify_candidates<R: SqlFileRow>(&self, min_pages: u64) -> Result<Vec<R>> {
        sparsify::list_sparsify_candidates(&*self.conn(), min_pages)
    }

    pub fn mark_sparsified_sparse(&self, file_id: FileId) -> Result<()> {
        sparsify::mark_sparsified_sparse(&*self.conn(), file_id)
    }

    pub fn mark_sparsified_error(&self, file_id: FileId) -> Result<()> {
        sparsify::mark_sparsified_error(&*self.conn(), file_id)
    }

    pub fn promote_unstageable_files(&self, retry_missing_sha: bool) -> Result<u64> {
        stage::promote_unstageable_files(&self.conn(), retry_missing_sha)
    }

    pub fn list_files_to_stage<R: SqlFileRow>(&self, retry_missing_sha: bool) -> Result<Vec<R>> {
        stage::list_files_to_stage(&self.conn(), retry_missing_sha)
    }

    pub fn mark_active_canonical(&self, file_id: FileId) -> Result<()> {
        dedup::mark_active_canonical(&*self.conn(), file_id)
    }

    pub fn promote_to_deduped(&self, file_id: FileId) -> Result<()> {
        dedup::promote_to_deduped(&*self.conn(), file_id)
    }

    pub fn promote_excluded_entries_to_deduped(&self) -> Result<u64> {
        dedup::promote_excluded_entries_to_deduped(&self.conn())
    }

        pub fn clear_check_with_canonical_completed(
        &self,
        sha1: &[u8; 20],
        size: u64,
    ) -> Result<()> {
        dedup::clear_check_with_canonical_completed(&*self.conn(), sha1, size)
    }

    pub fn promote_errored_pending_to_deduped(
        &self,
        sha1: &[u8; 20],
        size: u64,
    ) -> Result<u64> {
        dedup::promote_errored_pending_to_deduped(&*self.conn(), sha1, size)
    }

    pub fn count_check_with_canonical_completed(&self) -> Result<u64> {
        dedup::count_check_with_canonical_completed(&*self.conn())
    }

    pub fn count_active_canonicals(&self, sha1: &[u8; 20], size: u64) -> Result<u64> {
        dedup::count_active_canonicals(&*self.conn(), sha1, size)
    }

    pub fn promote_active_canonical_in_group(&self, sha1: &[u8; 20], size: u64) {
        dedup::promote_active_canonical_in_group(&*self.conn(), sha1, size)
    }

    pub fn count_electable_pending(&self, sha1: &[u8; 20], size: u64) -> Result<u64> {
        dedup::count_electable_pending(&*self.conn(), sha1, size)
    }

    pub fn list_filtered_in_group<R: SqlFileRow>(
        &self,
        sha1: &[u8; 20],
        size: u64,
    ) -> Result<Vec<R>> {
        dedup::list_filtered_in_group(&*self.conn(), sha1, size)
    }

    // TODO: Mark file and descendants in Phase
    pub fn set_canonical(&self, file_id: FileId, canonical_id: FileId) -> Result<()> {
        dedup::set_canonical(&*self.conn(), file_id, canonical_id)
    }

    pub fn mark_self_canonical(&self, file_id: FileId) -> Result<()> {
        dedup::mark_self_canonical(&*self.conn(), file_id)
    }

    pub fn load_runtime_state(&self) -> Result<Option<RuntimeState>> {
        inventory::load_runtime_state(&*self.conn())
    }

    pub fn save_runtime_state(&self, state: &RuntimeState) -> Result<()> {
        inventory::save_runtime_state(&mut *self.conn_mut(), state)
    }

    pub fn begin_archive_session(&self, archive_offset: u64) -> Result<i64> {
        tar_writer::begin_session(&*self.conn(), archive_offset)
    }

    pub fn stamp_archive_session_finished_at(&self, session_id: i64) -> Result<()> {
        tar_writer::stamp_session_finished_at(&*self.conn(), session_id)
    }

    pub fn finalize_archive_session(&self, session_id: i64) -> Result<()> {
        tar_writer::finalize_session(&*self.conn(), session_id)
    }

    pub fn mark_archive_session_aborted(&self, session_id: i64) -> Result<()> {
        tar_writer::mark_session_aborted(&*self.conn(), session_id)
    }

    pub fn abort_incomplete_archive_session(
        &self,
        session: &ArchiveSession,
    ) -> Result<()> {
        tar_writer::abort_incomplete_session(&*self.conn(), session)
    }

    pub fn promote_pending_archived(&self) -> Result<u64> {
        tar_writer::promote_pending_archived(&*self.conn())
    }

    pub fn mark_archive_session_pending(&self, file_id: FileId) -> Result<()> {
        tar_writer::mark_archive_session_pending(&*self.conn(), file_id)
    }

    pub fn clear_archive_session_pending(&self) -> Result<u64> {
        tar_writer::clear_archive_session_pending(&*self.conn())
    }

    pub fn open_archive_session(&self) -> Result<Option<ArchiveSession>> {
        tar_writer::open_session(&*self.conn())
    }

    pub fn has_finalized_archive_session(&self) -> Result<bool> {
        tar_writer::has_finalized_session(&*self.conn())
    }

    pub fn reset_archive_state(&self) -> Result<()> {
        tar_writer::reset_archive_state(&*self.conn())
    }

    pub fn clear_archive_meta(&self) -> Result<()> {
        meta::clear_archive_meta(&mut *self.conn_mut())
    }

    pub fn dump_meta(&self) -> Result<meta::MetaDump> {
        meta::dump_meta(&*self.conn())
    }

    pub fn set_archive_owner_policy(
        &self,
        policy: &crate::common::perms::OwnerGroupPolicy,
    ) -> Result<()> {
        meta::set_archive_owner_policy(&*self.conn(), policy)
    }

    pub fn get_archive_owner_policy(
        &self,
    ) -> Result<Option<crate::common::perms::OwnerGroupPolicy>> {
        meta::get_archive_owner_policy(&*self.conn())
    }

    pub fn set_archive_mode_changes(&self, changes: &str) -> Result<()> {
        meta::set_archive_mode_changes(&*self.conn(), changes)
    }

    pub fn get_archive_mode_changes(&self) -> Result<Option<String>> {
        meta::get_archive_mode_changes(&*self.conn())
    }

    pub fn sum_canonical_bytes_to_archive(&self, filter_sha: bool) -> Result<u64> {
        tar_writer::sum_canonical_bytes_to_archive(&*self.conn(), filter_sha)
    }

    pub fn sum_archived_canonical_bytes(&self, filter_sha: bool) -> Result<u64> {
        tar_writer::sum_archived_canonical_bytes(&*self.conn(), filter_sha)
    }

    /// Staged canonical ids ordered by extension / size / id for the archive pass.
    pub fn list_staged_canonical_ordered(&self, filter_sha: bool) -> Result<Vec<FileId>> {
        tar_writer::list_staged_canonical_ordered(&*self.conn(), filter_sha)
    }

    pub fn get_archive_bytes_in(&self) -> Result<u64> {
        tar_writer::get_archive_bytes_in(&*self.conn())
    }

    pub fn get_archive_bytes_out(&self) -> Result<Option<u64>> {
        tar_writer::get_archive_bytes_out(&*self.conn())
    }

    pub fn set_archive_bytes_in(&self, value: u64) -> Result<()> {
        tar_writer::set_archive_bytes_in(&*self.conn(), value)
    }

    pub fn set_archive_bytes_out(&self, value: u64) -> Result<()> {
        tar_writer::set_archive_bytes_out(&*self.conn(), value)
    }

    pub fn promote_ineligible_to_archived(&self, filter_sha: bool) -> Result<u64> {
        tar_writer::promote_ineligible_to_archived(&*self.conn(), filter_sha)
    }

    pub fn promote_remainder_to_archived(&self) -> Result<u64> {
        tar_writer::promote_remainder_to_archived(&*self.conn())
    }

    pub fn checkpoint(&self) -> Result<()> {
        common::checkpoint(&*self.conn())
    }

    // --- Extract pipeline ---

    pub fn install_initial_manifest(snapshot_path: &Path, db_path: &Path) -> Result<()> {
        scan::install_initial_manifest(snapshot_path, db_path)
    }

    pub fn normalize_installed_catalog(&self) -> Result<()> {
        scan::normalize_installed_catalog(&mut *self.conn_mut())
    }

    pub fn apply_snapshot_promote_unarchived(&self, snapshot_path: &Path) -> Result<u64> {
        scan::apply_snapshot_promote_unarchived(&*self.conn(), snapshot_path)
    }

    pub fn promote_extracted_to_unarchived(&self) -> Result<u64> {
        scan::promote_extracted_to_unarchived(&*self.conn())
    }

    pub fn flush_cached_payloads(&self, cache_dir: &Path) -> Result<u64> {
        scan::flush_cached_payloads(&*self.conn(), cache_dir)
    }

    pub fn count_missing_payloads(&self) -> Result<u64> {
        scan::count_missing_payloads(&*self.conn())
    }

    pub fn count_unconfirmed_extracted(&self) -> Result<u64> {
        scan::count_unconfirmed_extracted(&*self.conn())
    }

    pub fn count_extracted_canonical(&self) -> Result<u64> {
        scan::count_extracted_canonical(&*self.conn())
    }

    pub fn count_extracted_paths(&self) -> Result<u64> {
        scan::count_extracted_paths(&*self.conn())
    }

    pub fn count_non_appended_by_ftype(&self) -> Result<Vec<(String, u64)>> {
        scan::count_non_appended_by_ftype(&*self.conn())
    }

    pub fn load_extract_scan_state(&self) -> Result<extract::ExtractScanState> {
        scan::load_extract_scan_state(&*self.conn())
    }

    pub fn save_extract_scan_state(&self, state: &extract::ExtractScanState) -> Result<()> {
        scan::save_extract_scan_state(&mut *self.conn_mut(), state)
    }

    pub fn load_extract_runtime_state(&self) -> Result<Option<ExtractRuntimeState>> {
        extract::load_extract_runtime_state(&*self.conn())
    }

    pub fn save_extract_runtime_state(&self, state: &ExtractRuntimeState) -> Result<()> {
        extract::save_extract_runtime_state(&mut *self.conn_mut(), state)
    }

    pub fn record_snapshot_ingested(&self) -> Result<u32> {
        scan::record_snapshot_ingested(&mut *self.conn_mut())
    }

    pub fn list_files_to_restore<R: SqlFileRow>(&self) -> Result<Vec<R>> {
        extract::list_files_to_restore(&*self.conn())
    }

    pub fn list_files_to_rehash<R: SqlFileRow>(&self, batch_size: u64) -> Result<Vec<R>> {
        rehash::list_files_to_rehash(&self.conn(), batch_size)
    }

    pub fn skip_rehash(&self) -> Result<u64> {
        rehash::skip_rehash(&*self.conn())
    }

    pub fn init_extract_runtime_state(&self) -> Result<()> {
        scan::init_extract_runtime_state(&mut *self.conn_mut())
    }

    pub fn list_canonical_files_for_move<R: SqlFileRow>(
        &self, filter: bool, last_id: FileId, batch_size: u64
    ) -> Result<Vec<R>> {
        place::list_canonical_files_for_move(&self.conn(), filter, last_id, batch_size)
    }

    pub fn out_tree_is_built(&self) -> Result<bool> {
        place::out_tree_is_built(&self.conn())
    }

    pub fn dir_tree_is_built(&self) -> Result<bool> {
        place::dir_tree_is_built(&self.conn())
    }

    pub fn set_out_tree_built(&self) -> Result<()> {
        place::set_out_tree_built(&self.conn())
    }

    pub fn placement_prologue_done(&self) -> Result<bool> {
        place::placement_prologue_done(&self.conn())
    }

    pub fn set_placement_prologue_done(&self) -> Result<()> {
        place::set_placement_prologue_done(&self.conn())
    }

    pub fn set_file_new_name(&self, file_id: FileId, new_name: Option<&str>) -> Result<()> {
        place::set_file_new_name(&self.conn(), file_id, new_name)
    }

    pub fn set_dir_tree_built(&self) -> Result<()> {
        place::set_dir_tree_built(&self.conn())
    }

    pub fn list_materialized_entries<R: SqlFileRow>(
        &self,
        last_id: Option<FileId>,
        batch_size: u64,
        source_id: Option<i64>,
        only_dirs: Option<bool>,
    ) -> Result<Vec<R>> {
        place::list_materialized_entries(&self.conn(), last_id, batch_size, source_id, only_dirs)
    }

    pub fn insert_out_tree_rows(
        &self,
        rows: &[NewOutTreeRow],
    ) -> Result<Vec<OutTreeId>> {
        place::insert_out_tree_rows(&self.conn(), rows)
    }

    pub fn insert_ref_out_rows(&self, pairs: &[(OutTreeId, i64)]) -> Result<()> {
        place::insert_ref_out_rows(&self.conn(), pairs)
    }

    pub fn list_out_tree(
        &self,
        last_id: OutTreeId,
        batch_size: u64,
        source_id: Option<i64>,
        only_dir: Option<bool>,
    ) -> Result<Vec<OutTreeRecord>> {
        common::list_out_tree(&self.conn(), last_id, batch_size, source_id, only_dir)
    }

    pub fn list_out_tree_for_materialization<R: SqlFileRow>(
        &self, last_id: &OutTreeId, batch_size: u64)
        -> Result<Vec<(R, OutTreeRecord)>> {
        place::list_out_tree_for_materialization(&self.conn(), last_id, batch_size)
    }

    pub fn list_out_tree_for_hardlinks<R: SqlFileRow>(
        &self, last_id: &OutTreeId, batch_size: u64)
        -> Result<Vec<(R, OutTreeRecord, OutTreeRecord)>> {
        place::list_out_tree_for_hardlinks(&self.conn(), last_id, batch_size)
    }

    pub fn list_out_tree_others<R: SqlFileRow>(&self, last_id: &OutTreeId, batch_size: u64)
        -> Result<Vec<(R, OutTreeRecord)>> {
        place::list_out_tree_others(&self.conn(), last_id, batch_size)
    }

    pub fn count_out_tree_canonicals(&self, materialized: Option<bool>) -> Result<u64> {
        place::count_out_tree_canonicals(&self.conn(), materialized)
    }
    pub fn count_out_tree_hardlinks(&self, materialized: Option<bool>) -> Result<u64> {
        place::count_out_tree_hardlinks(&self.conn(), materialized)
    }

    pub fn count_out_tree_others(&self, materialized: Option<bool>) -> Result<u64> {
        place::count_out_tree_others(&self.conn(), materialized)
    }

    pub fn count_out_tree_rows(&self) -> Result<u64> {
        place::count_out_tree_rows(&self.conn())
    }

    pub fn count_ref_out_rows(&self) -> Result<u64> {
        place::count_ref_out_rows(&self.conn())
    }

    pub fn list_out_tree_for_linking<R: SqlFileRow>(
        &self, batch_size: u64, pending: bool)
        -> Result<Vec<(R, OutTreeRecord)>> {
        place::list_out_tree_for_linking(&self.conn(), batch_size, pending)
    }

    pub fn mark_all_canonical(&self) -> Result<u64> {
        place::mark_all_canonical(&self.conn())
    }

    pub fn mark_global_canonical(&self) -> Result<u64> {
        place::mark_global_canonical(&self.conn())
    }

    pub fn mark_source_canonical(&self, source_id: i64) -> Result<u64> {
        place::mark_source_canonical(&self.conn(), source_id)
    }

    pub fn apply_flags_to_files(&self) -> Result<(u64, u64, u64, u64, u64, u64)> {
        place::apply_flags_to_files(&self.conn())
    }

    // --- permissions (metadata restore) ---

    pub fn list_out_tree_for_permissions_non_dir<R: SqlFileRow>(
        &self, batch_size: u64)
        -> Result<Vec<(R, OutTreeRecord)>> {
        permissions::list_out_tree_for_permissions_non_dir::<R>(&self.conn(), batch_size)
    }

    pub fn list_out_tree_for_permissions_dirs<R: SqlFileRow>(
        &self, batch_size: u64)
        -> Result<Vec<(Option<R>, OutTreeRecord)>> {
        permissions::list_out_tree_for_permissions_dirs::<R>(&self.conn(), batch_size)
    }

    pub fn list_canonical_files_for_permissions<R: SqlFileRow>(&self, batch_size: u64)
        -> Result<Vec<R>> {
        permissions::list_canonical_files_for_permissions(&self.conn(), batch_size)
    }

    pub fn count_out_tree_for_permissions(&self) -> Result<u64> {
        permissions::count_out_tree_for_permissions_non_dir(&self.conn())
    }

    pub fn count_out_tree_for_permissions_dirs(&self) -> Result<u64> {
        permissions::count_out_tree_for_permissions_dirs(&self.conn())
    }

    pub fn apply_permissions_flags_to_files(&self) -> Result<(u64, u64)> {
        permissions::apply_permissions_flags_to_files(&self.conn())
    }

    // --- errors (persistent error log) ---

    pub fn insert_errors(&self, drafts: &[errors::RecordDraft]) -> Result<u64> {
        errors::insert_errors(&mut *self.conn_mut(), drafts)
    }

    pub fn get_record_by_id(&self, id: i64) -> Result<Option<errors::ErrorRecord>> {
        errors::get_record_by_id(&self.conn(), id)
    }

    pub fn get_records_by_file_id(&self, file_id: FileId)
        -> Result<Vec<errors::ErrorRecord>> {
        errors::get_records_by_file_id(&self.conn(), file_id)
    }

    pub fn get_records_by_out_tree_id(&self, out_tree_id: OutTreeId)
        -> Result<Vec<errors::ErrorRecord>> {
        errors::get_records_by_out_tree_id(&self.conn(), out_tree_id)
    }

    pub fn list_records(
        &self,
        scope: flags::ErrorScope,
        reemit: Option<(bool, i64)>,
        last_id: i64,
        batch_size: u64,
    ) -> Result<Vec<errors::ErrorRecord>> {
        errors::list_records(&self.conn(), scope, reemit, last_id, batch_size)
    }

    pub fn count_records(
        &self,
        scope: flags::ErrorScope,
        reemit: Option<(bool, i64)>,
    ) -> Result<u64> {
        errors::count_records(&self.conn(), scope, reemit)
    }

    // --- integrity checks across the database
    
    pub fn count_missing_dev_inode(&self) -> Result<u64> {
        integrity::count_missing_dev_inode(&self.conn())
    }
    pub fn list_missing_dev_inode<R: SqlFileRow>(
        &self, last_id: &FileId, batch_size: u64) -> Result<Vec<R>> {
        integrity::list_missing_dev_inode(&self.conn(), last_id, batch_size)
    }
    pub fn count_double_canonical_dev_inode_group(&self) -> Result<u64> {
        integrity::count_double_canonical_dev_inode_group(&self.conn())
    }
    pub fn list_double_canonical_dev_inode_group<R: SqlFileRow>(
        &self, last_id: &FileId, batch_size: u64)
        -> Result<Vec<R>> {
        integrity::list_double_canonical_dev_inode_group(&self.conn(), last_id, batch_size)
    }
    pub fn count_id_implication(&self) -> Result<u64> {
        integrity::count_id_implication(&self.conn())
    }
    pub fn count_missing_unix_infos(&self) -> Result<u64> {
        integrity::count_missing_unix_infos(&self.conn())
    }
}

/// Batching recorder for the persistent error log. Accumulates drafts and flushes
/// them in one transaction. When `enabled` is `false` (the `--no-errors` flag),
/// drafts are dropped.
///
/// The database is optional so the recorder can be *speculative*: errors may be
/// added before any database exists (e.g. while a scan is still locating the
/// catalog). Use [`Recorder::new`] when a database is already in hand, or
/// [`Recorder::speculative`] followed by [`Recorder::bind`] once one appears.
/// A [`Recorder::flush`] with no attached database logs the loss instead.
///
/// Auto-flush (on by default): once the buffered draft count reaches
/// [`crate::common::DEFAULT_AUTO_FLUSH_LIMIT`] a flush is attempted so bursts of
/// errors (e.g. an unreliable filesystem) don't grow the buffer without bound.
/// Auto-flush never propagates errors and retains the drafts for a later retry.
pub struct Recorder<'a> {
    db: Option<&'a Database>,
    buf: Vec<RecordDraft>,
    enabled: bool,
    auto_flush: bool,
    flush_limit: u64,
}

impl<'a> Recorder<'a> {
    /// Recorder tied to an already-open database.
    pub fn new(db: &'a Database, enabled: bool) -> Self {
        Self {
            db: Some(db),
            buf: Vec::new(),
            enabled,
            auto_flush: true,
            flush_limit: crate::common::DEFAULT_AUTO_FLUSH_LIMIT,
        }
    }

    /// Speculative recorder: buffers errors without a database. Attach one later
    /// with [`bind`]; until then a [`flush`] only reports that it could not store.
    pub fn speculative(enabled: bool) -> Self {
        Self {
            db: None,
            buf: Vec::new(),
            enabled,
            auto_flush: true,
            flush_limit: crate::common::DEFAULT_AUTO_FLUSH_LIMIT,
        }
    }

    /// Attach (or replace) the database reference. Buffered drafts stay buffered;
    /// call [`flush`] to persist them. The referenced database must outlive the
    /// recorder for the remainder of its use.
    pub fn bind(&mut self, db: &'a Database) {
        self.db = Some(db);
    }

    /// Drop the database reference, going back to speculative. Buffered drafts are
    /// preserved; a later [`bind`] + [`flush`] can still persist them.
    pub fn detach(&mut self) {
        self.db = None;
    }

    /// Disable/enable automatic flushing once the buffer exceeds the default limit.
    pub fn set_auto_flush(&mut self, on: bool) {
        self.auto_flush = on;
    }

    /// Override the auto-flush threshold (draft count at which a flush is tried).
    pub fn set_flush_limit(&mut self, limit: u64) {
        self.flush_limit = limit;
    }

    pub fn record(
        &mut self,
        file_id: Option<FileId>,
        out_tree_id: Option<OutTreeId>,
        phase: ErrorPhase,
        error: crate::error::FileStatError,
        flags: flags::ErrorFlags,
    ) {
        if !self.enabled {
            return;
        }
        self.buf.push(RecordDraft {
            file_id,
            out_tree_id,
            phase,
            error,
            flags,
        });
        self.try_auto_flush();
    }

    /// Session-scoped error: neither a file nor an out_tree row.
    ///
    /// The `SessionError` flag is always set, regardless of `flags`.
    pub fn record_session(
        &mut self,
        phase: ErrorPhase,
        error: crate::error::FileStatError,
        flags: flags::ErrorFlags,
    ) {
        self.record(
            None,
            None,
            phase,
            error,
            flags.with(flags::ErrorFlag::SessionError, true),
        );
    }

    /// File-scoped error (canonical, duplicate, or any file row).
    pub fn record_file(
        &mut self,
        file_id: FileId,
        phase: ErrorPhase,
        error: crate::error::FileStatError,
        flags: flags::ErrorFlags,
    ) {
        self.record(Some(file_id), None, phase, error, flags);
    }

    /// Out-tree-scoped error.
    pub fn record_out_tree(
        &mut self,
        out_tree_id: OutTreeId,
        phase: ErrorPhase,
        error: crate::error::FileStatError,
        flags: flags::ErrorFlags,
    ) {
        self.record(None, Some(out_tree_id), phase, error, flags);
    }

    pub fn push(&mut self, draft: RecordDraft) {
        if self.enabled {
            self.buf.push(draft);
            self.try_auto_flush();
        }
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Best-effort flush once the buffer has grown past the auto-flush limit.
    /// No-op while no database is attached (keeps buffering speculatively) and
    /// never propagates an error: on failure the drafts stay buffered for retry.
    fn try_auto_flush(&mut self) {
        if !self.auto_flush || self.db.is_none() {
            return;
        }
        if (self.buf.len() as u64) < self.flush_limit {
            return;
        }
        match self.flush() {
            Ok(_) => (),
            Err(e) => {
                // Retained by fill of flush() on failure; report once.
                tracing::error!(
                    error = %e,
                    pending = self.buf.len(),
                    "auto-flush failed; errors retained for retry"
                );
            }
        }
    }

    /// Flush buffered drafts in a single transaction. Clears the buffer on
    /// success and retains it on failure (so a retry can persist the same rows).
    /// Without an attached database, buffered drafts are kept and the failure to
    /// store is logged.
    pub fn flush(&mut self) -> Result<u64> {
        if self.buf.is_empty() {
            return Ok(0);
        }
        match self.db {
            None => {
                tracing::error!(
                    pending = self.buf.len(),
                    "Could not store the error, no db present"
                );
                Ok(0)
            }
            Some(db) => {
                let n = db.insert_errors(&self.buf)?;
                self.buf.clear();
                Ok(n)
            }
        }
    }

    /// Flush buffered drafts against a one-off database, without attaching it to
    /// the recorder. Useful when a reference would not live long enough to store
    /// (e.g. a database that is created and dropped within one frame).
    pub fn flush_into(&mut self, db: &Database) -> Result<u64> {
        if self.buf.is_empty() {
            return Ok(0);
        }
        let n = db.insert_errors(&self.buf)?;
        self.buf.clear();
        Ok(n)
    }
}

impl Drop for Recorder<'_> {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            // Best-effort flush so errors are not lost if a phase forgets to flush.
            // Log-only: a failing flush reports the count but never aborts.
            tracing::info!("flushing remaining errors to db");
            match self.flush() {
                Ok(n) if n > 0 => tracing::info!(flushed = n, "errors flushed"),
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "failed to flush remaining errors"),
            }
        }
    }
}
