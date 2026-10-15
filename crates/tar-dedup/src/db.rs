#![allow(rustdoc::private_intra_doc_links)] // facade methods link to the `db/*` module impl fns

use std::cell::{Ref, RefCell, RefMut};
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::common::perms::OwnerGroupPolicy;
use crate::config::{
    ExtractConfig, ExtractPipelinePhase, ExtractRuntimeState, PipelinePhase, RuntimeState,
};
use crate::db::flags::{FileFlag, FileFlags, OutTreeFlag, OutTreeFlags, SourceFlags};
use crate::db::types::{
    ArchiveSession, FileId, FilePhase, FilterExpression, NewFileRecord, NewOutTreeRow,
    OutTreeId, OutTreeRecord, SourceRecord,
};
use crate::error::{Error, FileStatError, Result, ToPanic};
pub mod flags;
pub mod types;

pub use errors::{ErrorPhase, ErrorRecord, RecordDraft};

mod common;
pub mod content_id;
pub(crate) mod dedup;
mod errors;
mod extract;
mod filter;
pub(crate) mod hash;
mod integrity;
mod inventory;
pub mod meta;
mod permissions;
pub mod place;
mod place_prologue;
pub(crate) mod rehash;
mod scan;
mod schema;
mod source;
pub(crate) mod sparsify;
mod stage;
mod stats;
mod tar_writer;

pub use common::SqlFileRow;
pub use extract::ExtractScanState;
pub use integrity::{
    ArchiveGate, archive_phase_check_count, extract_phase_check_count,
};
pub use meta::{MetaDump, MetaEntry, MetaKey, dump_meta};
pub use scan::ScanRemainderTally;
use crate::common::defaults::DEFAULT_AUTO_FLUSH_LIMIT;
use crate::db::dedup::CompareOutcome;
use crate::db::hash::HashingOutcome;
use crate::db::place::{CopyOutcome, MaterializeResult};
use crate::db::rehash::RehashOutcome;
use crate::db::sparsify::SparseOutcome;

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
        let add_info = !path.exists();
        let conn = Connection::open(path)?;
        schema::initialize(&conn)?;
        // Stamp the producing tool version + host, refreshed on every open so a
        // future schema migration can see which release last touched the db.
        if add_info {
            meta::set_tar_dedup_version(&conn, crate::common::TOOL_VERSION)?;
            meta::set_tar_dedup_os(&conn, &crate::common::host_os_string())?;
            meta::set_db_version(&conn, crate::common::DB_VERSION)?;
        } else {
            // Keen-awareness for the hand-maintained `DB_VERSION`: a work-db
            // stamped with a different schema version has no migration path yet,
            // so surface it loudly; a version-less (pre-`db_version`) db is
            // adopted silently.
            match meta::get_db_version(&conn)? {
                Some(v) if v != crate::common::DB_VERSION => tracing::warn!(
                    "work-db schema version mismatch: db has {v}, this build expects {}; \
                     no migration path exists — proceed at your own risk",
                    crate::common::DB_VERSION,
                ),
                Some(_) => (),
                None => meta::set_db_version(&conn, crate::common::DB_VERSION)?,
            }
        }
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
        let file_id = inventory::file_id_by_abs_path(&tx, record.fs_path())?.ok_or_else(|| {
            Error::Config(
                "insert_file_and_ref: file missing after insert".into(),
            )
        })?;
        flags::insert_ref(&tx, source_id, file_id)?;
        tx.commit()?;
        Ok(inserted)
    }

    pub fn add_get_source(
        &self,
        abs_path: &Path,
        source_kind: &str,
        line: Option<u64>,
        original_path: &Path,
        abs_path_raw: &Path,
        original_path_raw: &Path,
        flags: SourceFlags)
        -> Result<i64> {
        source::add_get_source(
            &*self.conn(),
            abs_path,
            source_kind,
            line,
            original_path,
            abs_path_raw,
            original_path_raw,
            flags,
        )
    }

    pub fn find_overlapping_source(
        &self,
        abs_path: &Path,
        no_recursion: bool)
        -> Result<Option<(i64, PathBuf)>> {
        source::find_overlapping_source(&*self.conn(), abs_path, no_recursion)
    }

    pub fn list_sources(
        &self,
        only_dirs: Option<bool>,
        starting_id: i64,
        batch_size: u64)
        -> Result<Vec<SourceRecord>> {
        source::list_sources(&self.conn(), only_dirs, starting_id, batch_size)
    }

    pub fn get_file_by_id<R: SqlFileRow>(&self, file_id: FileId) -> Result<Option<R>> {
        common::get_file_by_id(&*self.conn(), file_id)
    }

    pub fn count_entries(&self) -> Result<u64> {
        common::count_entries(&*self.conn())
    }

    pub fn count_files_in_phase(&self, phase: FilePhase) -> Result<u64> {
        common::count_files_entries_in_phase(&*self.conn(), phase)
    }

    pub fn files_in_phase<R: SqlFileRow>(&self, phase: FilePhase) -> Result<Vec<R>> {
        common::list_files_entries_in_phase(&*self.conn(), phase)
    }

    pub fn global_mark_phase(&self, phase: FilePhase) -> Result<u64> {
        common::global_mark_phase(&self.conn(), phase)
    }

    pub fn mark_file_phase(&self, file_id: FileId, phase: FilePhase) -> Result<()> {
        common::mark_phase(&*self.conn(), file_id, phase)
    }

    pub fn resolve_numeric_ids(&self, recorder: &mut Recorder) -> Result<()> {
        inventory::resolve_numeric_ids(&*self.conn(), recorder)
    }

    pub fn purge_entries(&self) -> Result<u64> {
        inventory::purge_entries(&self.conn())
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

    pub fn promote_unhasheable_files(
        &self, eager_filter: bool, detect_hardlinks: bool) -> Result<u64> {
        hash::promote_unhasheable_files(&self.conn(), eager_filter, detect_hardlinks)
        
    }

    /// Run `f` inside a transaction. `f` receives `&Connection` (the
    /// transaction derefs to one) so typed setters work inside or outside a txn.
    pub fn with_transaction<T>(&self, f: impl FnOnce(&Connection) -> Result<T>)
        -> Result<T> {
        let mut conn = self.conn_mut();
        let tx = conn.transaction().to_panic()?;
        let out = f(&tx)?;
        tx.commit().to_panic()?;
        Ok(out)
    }
    
    /// See [`hash::create_hash_queue`].
    pub fn create_hash_queue(&self) -> Result<()> {
        hash::create_hash_queue(&*self.conn())
    }

    /// See [`hash::populate_hash_queue`].
    pub fn populate_hash_queue(&self, eager_filter: bool, detect_hardlinks: bool) -> Result<u64> {
        hash::populate_hash_queue(&*self.conn(), eager_filter, detect_hardlinks)
    }

    /// See [`hash::pull_pending_hash_rows`].
    pub fn pull_pending_hash_rows<R: SqlFileRow>(&self, index: u64, limit: u64)
        -> Result<Vec<(u64, R)>> {
        hash::pull_pending_hash_rows(&*self.conn(), index, limit)
    }

    /// See [`hash::drop_hash_queue`].
    pub fn drop_hash_queue(&self) -> Result<()> {
        hash::drop_hash_queue(&*self.conn())
    }

    pub fn count_all_hashable_files(
        &self,
        eager_filter: bool,
        detect_hardlinks: bool)
        -> Result<u64> {
        hash::count_all_hashable_files(&*self.conn(), eager_filter, detect_hardlinks)
    }

    pub fn count_pending_hashable_files(&self, eager_filter: bool, detect_hardlinks: bool) 
        -> Result<u64> {
        hash::count_pending_hashable_files(&self.conn(), eager_filter, detect_hardlinks)
    }


    pub fn update_file_inspection_per_id(
        &self,
        file_id: FileId,
        digest: [u8; 20],
        sparse_count: u64,
        update_hardlinks: bool)
        -> Result<()> {
        hash::update_file_inspection_per_id(
            &*self.conn(),
            file_id,
            digest,
            sparse_count,
            update_hardlinks,
        )
    }

    pub fn ingest_hash_outcome(
        &self, results: &Vec<HashingOutcome>, update_hardlinks: bool)
        -> Result<u64> {
        hash::ingest_hash_outcome(&mut self.conn_mut(), results, update_hardlinks)
    }

    pub fn promote_singleton_filtered_to_deduped(&self, eager_filter: bool) -> Result<u64> {
        dedup::promote_singleton_filtered_to_deduped(&*self.conn(), eager_filter)
    }

    pub fn promote_ineligible_entries_to_dedup(&self, eager_filter: bool) -> Result<u64> {
        dedup::promote_ineligible_entries_to_dedup(&*self.conn(), eager_filter)
    }

    /// See [`dedup::promote_hash_mode_to_dedup`].
    pub fn promote_hash_mode_to_dedup(&self, eager_filter: bool) -> Result<u64> {
        dedup::promote_hash_mode_to_dedup(&mut self.conn_mut(), eager_filter)
    }

    /// See [`dedup::promote_none_mode_to_dedup`].
    pub fn promote_none_mode_to_dedup(
        &self, eager_filter: bool, detect_hardlinks: bool) -> Result<u64> {
        dedup::promote_none_mode_to_dedup(&mut self.conn_mut(), eager_filter, detect_hardlinks)
    }

    /// See [`dedup::count_dedup_eligible_total`].
    pub fn count_dedup_eligible_total(&self, eager_filter: bool) -> Result<u64> {
        dedup::count_dedup_eligible_total(&*self.conn(), eager_filter)
    }

    /// See [`dedup::count_dedup_eligible_position`].
    pub fn count_dedup_eligible_position(&self, eager_filter: bool) -> Result<u64> {
        dedup::count_dedup_eligible_position(&*self.conn(), eager_filter)
    }

    /// See [`dedup::create_temp_dedup_table`].
    pub fn create_temp_dedup_table(&self) -> Result<()> {
        dedup::create_temp_dedup_table(&*self.conn())
    }

    /// See [`dedup::drop_temp_dedup_table`].
    pub fn drop_temp_dedup_table(&self) -> Result<()> {
        dedup::drop_temp_dedup_table(&*self.conn())
    }

    /// See [`dedup::populate_temp_table`].
    pub fn populate_temp_table(&self, eager_filter: bool) -> Result<u64> {
        dedup::populate_temp_table(&*self.conn(), eager_filter)
    }

    /// See [`dedup::count_pending_dedup_groups`].
    pub fn count_pending_dedup_groups(&self) -> Result<u64> {
        dedup::count_pending_dedup_groups(&*self.conn())
    }

    /// See [`dedup::count_dedup_phase_total`].
    pub fn count_dedup_phase_total(&self, eager_filter: bool) -> Result<u64> {
        dedup::count_dedup_phase_total(&*self.conn(), eager_filter)
    }

    /// See [`dedup::count_dedup_phase_position`].
    pub fn count_dedup_phase_position(&self, eager_filter: bool) -> Result<u64> {
        dedup::count_dedup_phase_position(&*self.conn(), eager_filter)
    }

    /// See [`dedup::searching_to_finished`].
    pub fn searching_to_finished(&self, eager_filter: bool) -> Result<u64> {
        dedup::searching_to_finished(&mut *self.conn_mut(), eager_filter)
    }

    /// See [`dedup::finish_to_error`].
    pub fn finish_to_error(&self, eager_filter: bool) -> Result<(u64, u64)> {
        dedup::finish_to_error(&mut *self.conn_mut(), eager_filter)
    }

    /// See [`dedup::finish_to_done`].
    pub fn finish_to_done(&self, eager_filter: bool) -> Result<u64> {
        dedup::finish_to_done(&mut *self.conn_mut(), eager_filter)
    }

    /// See [`dedup::finish_to_ready`].
    pub fn finish_to_ready(&self, eager_filter: bool) -> Result<u64> {
        dedup::finish_to_ready(&mut *self.conn_mut(), eager_filter)
    }

    /// See [`dedup::ready_to_searching`].
    pub fn ready_to_searching(&self, eager_filter: bool) -> Result<(u64, u64)> {
        dedup::ready_to_searching(&mut *self.conn_mut(), eager_filter)
    }

    /// See [`dedup::list_pending_comparisons`].
    pub fn list_pending_comparisons<R: SqlFileRow>(
        &self, eager_filter: bool, last_candidate_id: u64, limit: u64)
        -> Result<Vec<(R, R)>> {
        dedup::list_pending_comparisons::<R>(
            &*self.conn(), eager_filter, last_candidate_id, limit)
    }

    pub fn mark_inflight(&self, ids: &[FileId]) -> Result<()> {
        dedup::mark_inflight(&*self.conn(), ids)
    }

    pub fn unmark_inflight(&self, ids: &[FileId]) -> Result<()> {
        dedup::unmark_inflight(&*self.conn(), ids)
    }

    pub fn set_canonical(&self, file_id: FileId, canonical_id: FileId) -> Result<()> {
        dedup::set_canonical(&*self.conn(), file_id, canonical_id)
    }

    pub fn mark_self_canonical(&self, file_id: FileId) -> Result<()> {
        dedup::mark_self_canonical(&*self.conn(), file_id)
    }

    pub fn count_check_with_canonical_completed(&self) -> Result<u64> {
        dedup::count_check_with_canonical_completed(&*self.conn())
    }

    pub fn ingest_compare_outcome(
        &self, results: &Vec<CompareOutcome>)
        -> Result<u64> {
        dedup::ingest_compare_outcome(&mut self.conn_mut(), &results)
    }

    pub fn add_include_pattern(&self, from: &str, line: Option<u64>, query: &str) -> Result<u64> {
        filter::add_include_pattern_archive(&*self.conn(), from, line, query)
    }

    pub fn add_exclude_pattern(&self, from: &str, line: Option<u64>, query: &str) -> Result<u64> {
        filter::add_exclude_pattern_archive(&*self.conn(), from, line, query)
    }

    pub fn count_filters_archive(&self, exclude: Option<bool>) -> Result<u64> {
        filter::count_filters(&*self.conn(), exclude)
    }

    pub fn get_filters_archive(&self, exclude: bool) -> Result<Vec<FilterExpression>> {
        filter::get_filters(&*self.conn(), exclude)
    }

    pub fn apply_no_filter_archive(&self) -> Result<u64> {
        filter::apply_no_filter(&*self.conn())
    }

    pub fn get_rows_to_filter_archive<R: SqlFileRow>(
        &self, last_id: &FileId, eager_filter: bool, batch_size: u64)
        -> Result<Vec<R>> {
        filter::get_rows_to_filter_archive(&*self.conn(), last_id, eager_filter, batch_size)
    }

    pub fn apply_filter_result_archive<I: Iterator<Item = (FileId, i64, i64)>>(
        &self,
        results: I)
        -> Result<u64> {
        filter::apply_filter_result_archive(&mut *self.conn_mut(), results)
    }

    pub fn clear_extract_filters(&self) -> Result<()> {
        filter::clear_extract_filters(&mut *self.conn_mut())
    }

    pub fn add_include_pattern_extract(&self, from: &str, line: Option<u64>, query: &str)
        -> Result<u64> {
        filter::add_include_pattern_extract(&*self.conn(), from, line, query)
    }

    pub fn add_exclude_pattern_extract(&self, from: &str, line: Option<u64>, query: &str)
        -> Result<u64> {
        filter::add_exclude_pattern_extract(&*self.conn(), from, line, query)
    }

    pub fn count_filters_extract(&self, exclude: Option<bool>) -> Result<u64> {
        filter::count_filters_extract(&*self.conn(), exclude)
    }

    pub fn get_filters_extract(&self, exclude: bool) -> Result<Vec<FilterExpression>> {
        filter::get_filters_extract(&*self.conn(), exclude)
    }

    pub fn apply_no_filter_extract(&self) -> Result<u64> {
        filter::apply_no_filter_extract(&*self.conn())
    }

    pub fn get_rows_to_filter_extract<R: SqlFileRow>(
        &self, last_id: FileId, batch_size: u64)
        -> Result<Vec<R>> {
        filter::get_rows_to_filter_extract(&*self.conn(), last_id, batch_size)
    }

    pub fn apply_filter_result_extract<I: Iterator<Item = (FileId, i64, i64)>>(
        &self, results: I) -> Result<u64> {
        filter::apply_filter_result_extract(&mut *self.conn_mut(), results)
    }

    pub fn fix_up_canonical_flag(&self) -> Result<(u64, u64)> {
        filter::fix_up_canonical_flag(&mut *self.conn_mut())
    }

    pub fn promote_deduped_to_sparsified(&self) -> Result<u64> {
        sparsify::promote_deduped_to_sparsified(&*self.conn())
    }

    pub fn promote_non_sparsify_candidates_to_sparsified(&self, min_pages: u64) -> Result<u64> {
        sparsify::promote_non_sparsify_candidates_to_sparsified(&*self.conn(), min_pages)
    }

    /// See [`sparsify::create_sparsify_queue`].
    pub fn create_sparsify_queue(&self) -> Result<()> {
        sparsify::create_sparsify_queue(&*self.conn())
    }

    /// See [`sparsify::populate_sparsify_queue`].
    pub fn populate_sparsify_queue(&self, min_pages: u64) -> Result<u64> {
        sparsify::populate_sparsify_queue(&*self.conn(), min_pages)
    }

    /// See [`sparsify::pull_pending_sparsify_rows`].
    pub fn pull_pending_sparsify_rows<R: SqlFileRow>(&self, index: u64, limit: u64)
        -> Result<Vec<(u64, R)>> {
        sparsify::pull_pending_sparsify_rows(&*self.conn(), index, limit)
    }

    /// See [`sparsify::drop_sparsify_queue`].
    pub fn drop_sparsify_queue(&self) -> Result<()> {
        sparsify::drop_sparsify_queue(&*self.conn())
    }

    /// See [`sparsify::count_all_sparsify_candidates`].
    pub fn count_all_sparsify_candidates(&self, min_pages: u64) -> Result<u64> {
        sparsify::count_all_sparsify_candidates(&*self.conn(), min_pages)
    }

    /// See [`sparsify::count_pending_sparsify_candidates`].
    pub fn count_pending_sparsify_candidates(&self, min_pages: u64) -> Result<u64> {
        sparsify::count_pending_sparsify_candidates(&*self.conn(), min_pages)
    }

    /// See [`sparsify::ingest_sparsify_outcome`].
    pub fn ingest_sparsify_outcome(&self, results: &Vec<SparseOutcome>) -> Result<u64> {
        sparsify::ingest_sparsify_outcome(&mut self.conn_mut(), results)
    }

    pub fn promote_unstageable_files(&self) -> Result<u64> {
        stage::promote_unstageable_files(&self.conn())
    }

    pub fn count_all_stage_candidates(&self) -> Result<u64> {
        stage::count_all_stage_candidates(&self.conn())
    }

    pub fn list_files_to_stage_after<R: SqlFileRow>(
        &self, last_id: &FileId, limit: u64) -> Result<Vec<R>> {
        stage::list_files_to_stage_after(&self.conn(), last_id, limit)
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

    pub fn abort_incomplete_archive_session(&self, session: &ArchiveSession) -> Result<()> {
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

    pub fn dump_meta(&self) -> Result<MetaDump> {
        dump_meta(&*self.conn())
    }

    /// See [`meta::get_tar_dedup_version`].
    pub fn get_tar_dedup_version(&self) -> Result<Option<String>> {
        meta::get_tar_dedup_version(&*self.conn())
    }

    /// See [`meta::get_tar_dedup_os`].
    pub fn get_tar_dedup_os(&self) -> Result<Option<String>> {
        meta::get_tar_dedup_os(&*self.conn())
    }

    /// See [`meta::get_db_version`].
    pub fn get_db_version(&self) -> Result<Option<String>> {
        meta::get_db_version(&*self.conn())
    }

    /// See [`meta::get_archive_phase`].
    pub fn get_archive_phase(&self) -> Result<Option<PipelinePhase>> {
        meta::get_archive_phase(&*self.conn())
    }

    /// See [`meta::get_extract_phase`].
    pub fn get_extract_phase(&self) -> Result<Option<ExtractPipelinePhase>> {
        meta::get_extract_phase(&*self.conn())
    }

    /// See [`meta::get_archive_snapshot_taken_at`].
    pub fn get_archive_snapshot_taken_at(&self) -> Result<Option<chrono::DateTime<chrono::Utc>>> {
        meta::get_archive_snapshot_taken_at(&*self.conn())
    }

    /// See [`meta::get_tar_writer_bytes_in`].
    pub fn get_tar_writer_bytes_in(&self) -> Result<Option<u64>> {
        meta::get_tar_writer_bytes_in(&*self.conn())
    }

    /// See [`meta::get_tar_writer_bytes_out`].
    pub fn get_tar_writer_bytes_out(&self) -> Result<Option<u64>> {
        meta::get_tar_writer_bytes_out(&*self.conn())
    }

    pub fn set_archive_owner_policy(
        &self,
        policy: &OwnerGroupPolicy)
        -> Result<()> {
        meta::set_archive_owner_policy(&*self.conn(), policy)
    }

    pub fn get_archive_owner_policy(&self) -> Result<Option<OwnerGroupPolicy>> {
        meta::get_archive_owner_policy(&*self.conn())
    }

    pub fn set_archive_mode_changes(&self, changes: &str) -> Result<()> {
        meta::set_archive_mode_changes(&*self.conn(), changes)
    }

    pub fn get_archive_mode_changes(&self) -> Result<Option<String>> {
        meta::get_archive_mode_changes(&*self.conn())
    }

    pub fn set_archive_transform(&self, transform: &str) -> Result<()> {
        meta::set_archive_transform(&*self.conn(), transform)
    }

    pub fn get_archive_transform(&self) -> Result<Option<String>> {
        meta::get_archive_transform(&*self.conn())
    }

    pub fn set_archive_config(&self, config: &crate::config::ArchiveConfig) -> Result<()> {
        meta::set_archive_config(&*self.conn(), config)
    }

    pub fn get_archive_config(&self) -> Result<Option<crate::config::ArchiveConfig>> {
        meta::get_archive_config(&*self.conn())
    }

    pub fn set_extract_config(&self, config: &ExtractConfig) -> Result<()> {
        meta::set_extract_config(&*self.conn(), config)
    }

    pub fn get_extract_config(&self) -> Result<Option<ExtractConfig>> {
        meta::get_extract_config(&*self.conn())
    }

    pub fn sum_canonical_bytes_to_archive(&self) -> Result<u64> {
        tar_writer::sum_canonical_bytes_to_archive(&*self.conn())
    }

    pub fn sum_archived_canonical_bytes(&self) -> Result<u64> {
        tar_writer::sum_archived_canonical_bytes(&*self.conn())
    }

    pub fn promote_to_archived(&self, id: &FileId) -> Result<u64> {
        tar_writer::promote_to_archived(&self.conn(), id)
    }

    /// See [`tar_writer::create_archive_queue`].
    pub fn create_archive_queue(&self) -> Result<()> {
        tar_writer::create_archive_queue(&*self.conn())
    }

    /// See [`tar_writer::populate_archive_queue`].
    pub fn populate_archive_queue(&self, order: crate::cli::ArchiveOrdering) -> Result<u64> {
        tar_writer::populate_archive_queue(&*self.conn(), order)
    }

    /// See [`tar_writer::pull_pending_archive_rows`].
    pub fn pull_pending_archive_rows<R: SqlFileRow>(&self, index: u64, limit: u64)
        -> Result<Vec<(u64, R)>> {
        tar_writer::pull_pending_archive_rows(&*self.conn(), index, limit)
    }

    /// See [`tar_writer::drop_archive_queue`].
    pub fn drop_archive_queue(&self) -> Result<()> {
        tar_writer::drop_archive_queue(&*self.conn())
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

    pub fn promote_ineligible_to_archived(&self) -> Result<u64> {
        tar_writer::promote_ineligible_to_archived(&*self.conn())
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

    pub fn tally_scan_remainders(&self, eager_filter: bool) -> Result<ScanRemainderTally> {
        scan::tally_scan_remainders(&self.conn(), eager_filter)
    }

    pub fn promote_filter_excluded_remainders(&self) -> Result<u64> {
        scan::promote_filter_excluded_remainders(&self.conn())
    }

    pub fn promote_scan_remainders(&self) -> Result<u64> {
        scan::promote_scan_remainders(&*self.conn())
    }

    pub fn check_cached_payloads(&self, cache_dir: &Path) -> Result<(u64, u64)> {
        scan::check_cached_payloads(&mut *self.conn_mut(), cache_dir)
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

    /// `(ftype, count)` for every row in `files`.
    pub fn inspect_by_ftype(&self) -> Result<Vec<(String, u64)>> {
        stats::count_by_ftype(&*self.conn())
    }

    /// `(included, excluded_by_include, excluded_by_exclude, unfiltered)`.
    pub fn inspect_filter_buckets(&self) -> Result<(u64, u64, u64, u64)> {
        stats::count_filter_buckets(&*self.conn())
    }

    /// Rows with a content hash.
    pub fn inspect_count_hashed(&self) -> Result<u64> {
        stats::count_hashed(&*self.conn())
    }

    /// Rows carrying `flag`.
    pub fn inspect_count_flag(&self, flag: FileFlag) -> Result<u64> {
        stats::count_flag(&*self.conn(), flag)
    }

    /// Self-canonical payloads whose `(sha1, size)` group has exactly one member.
    pub fn inspect_count_canonical_singletons(&self) -> Result<u64> {
        stats::count_canonical_singletons(&*self.conn())
    }

    /// Rows pointing at themselves as canonical payload.
    pub fn inspect_count_canonical_files(&self) -> Result<u64> {
        stats::count_canonical_files(&*self.conn())
    }

    /// Sum of `size` over all rows.
    pub fn inspect_sum_bytes_all(&self) -> Result<u64> {
        stats::sum_bytes_all(&*self.conn())
    }

    /// Sum of `size` over self-canonical payloads.
    pub fn inspect_sum_bytes_canonical(&self) -> Result<u64> {
        stats::sum_bytes_canonical(&*self.conn())
    }

    /// Sum of `size` over appended payloads.
    pub fn inspect_sum_bytes_archived(&self) -> Result<u64> {
        stats::sum_bytes_archived(&*self.conn())
    }

    /// `(started_at, finished_at, session_count)` over finalized sessions.
    pub fn inspect_session_bounds(&self) -> Result<(Option<String>, Option<String>, u64)> {
        stats::session_bounds(&*self.conn())
    }

    pub fn should_extract_canonical_id(&self, id: FileId) -> Result<bool> {
        scan::should_extract_canonical_id(&self.conn(), id)
    }

    pub fn load_extract_scan_state(&self) -> Result<ExtractScanState> {
        scan::load_extract_scan_state(&*self.conn())
    }

    pub fn save_extract_scan_state(&self, state: &ExtractScanState) -> Result<()> {
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

    /// See [`rehash::count_files_to_rehash`].
    pub fn count_files_to_rehash(&self) -> Result<u64> {
        rehash::count_files_to_rehash(&*self.conn())
    }

    /// See [`rehash::count_rehashed_files`].
    pub fn count_rehashed_files(&self) -> Result<u64> {
        rehash::count_rehashed_files(&*self.conn())
    }

    /// See [`rehash::promote_unrehashable_files`].
    pub fn promote_unrehashable_files(&self) -> Result<u64> {
        rehash::promote_unrehashable_files(&*self.conn())
    }

    /// See [`rehash::create_rehash_queue`].
    pub fn create_rehash_queue(&self) -> Result<()> {
        rehash::create_rehash_queue(&*self.conn())
    }

    /// See [`rehash::populate_rehash_queue`].
    pub fn populate_rehash_queue(&self) -> Result<u64> {
        rehash::populate_rehash_queue(&*self.conn())
    }

    /// See [`rehash::pull_pending_rehash_rows`].
    pub fn pull_pending_rehash_rows<R: SqlFileRow>(&self, index: u64, limit: u64)
        -> Result<Vec<(u64, R)>> {
        rehash::pull_pending_rehash_rows(&*self.conn(), index, limit)
    }

    /// See [`rehash::drop_rehash_queue`].
    pub fn drop_rehash_queue(&self) -> Result<()> {
        rehash::drop_rehash_queue(&*self.conn())
    }

    /// See [`rehash::ingest_rehash_outcome`].
    pub fn ingest_rehash_outcome(&self, results: &Vec<RehashOutcome>) -> Result<u64> {
        rehash::ingest_rehash_outcome(&mut self.conn_mut(), results)
    }

    /// See [`rehash::skip_rehash`].
    pub fn skip_rehash(&self) -> Result<u64> {
        rehash::skip_rehash(&*self.conn())
    }

    pub fn init_extract_runtime_state(&self) -> Result<()> {
        scan::init_extract_runtime_state(&mut *self.conn_mut())
    }

    /// See [`place::count_files_to_move`].
    pub fn count_files_to_move(&self, filter: bool) -> Result<u64> {
        place::count_files_to_move(&self.conn(), filter)
    }

    /// See [`place::count_moved_files`].
    pub fn count_moved_files(&self, filter: bool) -> Result<u64> {
        place::count_moved_files(&self.conn(), filter)
    }

    /// See [`place::create_canonical_move_queue`].
    pub fn create_canonical_move_queue(&self) -> Result<()> {
        place::create_canonical_move_queue(&self.conn())
    }

    /// See [`place::populate_canonical_move_queue`].
    pub fn populate_canonical_move_queue(&self, filter: bool) -> Result<u64> {
        place::populate_canonical_move_queue(&self.conn(), filter)
    }

    /// See [`place::pull_canonical_move_queue`].
    pub fn pull_canonical_move_queue<R: SqlFileRow>(&self, index: u64, limit: u64)
        -> Result<Vec<(u64, R)>> {
        place::pull_canonical_move_queue(&self.conn(), index, limit)
    }

    /// See [`place::drop_canonical_move_queue`].
    pub fn drop_canonical_move_queue(&self) -> Result<()> {
        place::drop_canonical_move_queue(&self.conn())
    }

    /// See [`place::create_materialize_queue`].
    pub fn create_materialize_queue(&self) -> Result<()> {
        place::create_materialize_queue(&self.conn())
    }

    /// See [`place::populate_materialize_queue`].
    pub fn populate_materialize_queue(&self) -> Result<u64> {
        place::populate_materialize_queue(&self.conn())
    }

    /// See [`place::pull_materialize_queue`].
    pub fn pull_materialize_queue<R: SqlFileRow>(&self, index: u64, limit: u64)
        -> Result<Vec<(u64, R, OutTreeRecord)>> {
        place::pull_materialize_queue(&self.conn(), index, limit)
    }

    /// See [`place::drop_materialize_queue`].
    pub fn drop_materialize_queue(&self) -> Result<()> {
        place::drop_materialize_queue(&self.conn())
    }

    pub fn list_canonical_files_for_move<R: SqlFileRow>(
        &self,
        filter: bool,
        last_id: FileId,
        batch_size: u64)
        -> Result<Vec<R>> {
        place::list_canonical_files_for_move(&self.conn(), filter, last_id, batch_size)
    }

    pub fn out_tree_is_built(&self) -> Result<bool> {
        place_prologue::out_tree_is_built(&self.conn())
    }

    pub fn dir_tree_is_built(&self) -> Result<bool> {
        place::dir_tree_is_built(&self.conn())
    }

    pub fn set_out_tree_built(&self) -> Result<()> {
        place_prologue::set_out_tree_built(&self.conn())
    }

    pub fn placement_prologue_done(&self) -> Result<bool> {
        place_prologue::placement_prologue_done(&self.conn())
    }

    pub fn set_placement_prologue_done(&self) -> Result<()> {
        place_prologue::set_placement_prologue_done(&self.conn())
    }

    pub fn set_file_new_name(&self, file_id: FileId, new_name: Option<&str>) -> Result<()> {
        place_prologue::set_file_new_name(&self.conn(), file_id, new_name)
    }

    pub fn set_dir_tree_built(&self) -> Result<()> {
        place::set_dir_tree_built(&self.conn())
    }

    pub fn list_materialized_entries<R: SqlFileRow>(
        &self,
        last_id: FileId,
        batch_size: u64,
        source_id: Option<i64>,
        only_dirs: Option<bool>,
        only_valid_new_name: bool,
        apply_filters: bool)
        -> Result<Vec<R>> {
        place_prologue::list_materialized_entries(
            &self.conn(), last_id, batch_size, source_id, only_dirs,
            only_valid_new_name, apply_filters)
    }

    pub fn insert_out_tree_rows(&self, rows: &[NewOutTreeRow]) -> Result<Vec<OutTreeId>> {
        place_prologue::insert_out_tree_rows(&self.conn(), rows)
    }

    pub fn insert_ref_out_rows(&self, pairs: &[(OutTreeId, i64)]) -> Result<()> {
        place::insert_ref_out_rows(&self.conn(), pairs)
    }

    pub fn list_out_tree(
        &self,
        last_id: OutTreeId,
        batch_size: u64,
        source_id: Option<i64>,
        only_dir: Option<bool>)
        -> Result<Vec<OutTreeRecord>> {
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

    pub fn list_out_tree_for_linking<R: SqlFileRow>(&self, batch_size: u64, pending: bool)
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

    pub fn ingest_results_link_tree(&self, results: &Vec<(OutTreeId, Option<FileStatError>)>)
                                    -> Result<u64> {
        place::ingest_results_link_tree(&mut self.conn_mut(), results)
    }

    pub fn ingest_materialize_results(
        &self,
        results: &Vec<std::result::Result<MaterializeResult, (OutTreeId, Error)>>,
        is_hardlink: bool,
        set_reflink: bool)
        -> Result<()> {
        place::ingest_materialize_results(&mut self.conn_mut(), results, is_hardlink, set_reflink)
    }

    pub fn ingest_copy_results(&self, results: &Vec<CopyOutcome>) -> Result<u64> {
        place::ingest_copy_results(&mut self.conn_mut(), results)
    }

    // --- permissions (metadata restore) ---

    pub fn list_out_tree_for_permissions_non_dir<R: SqlFileRow>(&self, batch_size: u64)
        -> Result<Vec<(R, OutTreeRecord)>> {
        permissions::list_out_tree_for_permissions_non_dir::<R>(&self.conn(), batch_size)
    }

    pub fn list_out_tree_for_permissions_dirs<R: SqlFileRow>(&self, batch_size: u64)
        -> Result<Vec<(Option<R>, OutTreeRecord)>> {
        permissions::list_out_tree_for_permissions_dirs::<R>(&self.conn(), batch_size)
    }

    pub fn list_canonical_files_for_permissions<R: SqlFileRow>(&self, batch_size: u64)
        -> Result<Vec<R>> {
        permissions::list_canonical_files_for_permissions(&self.conn(), batch_size)
    }

    /// See [`permissions::count_out_tree_for_permissions_non_dir`].
    pub fn count_out_tree_for_permissions(&self) -> Result<(u64, u64)> {
        permissions::count_out_tree_for_permissions_non_dir(&self.conn())
    }

    /// See [`permissions::count_out_tree_for_permissions_dirs`].
    pub fn count_out_tree_for_permissions_dirs(&self) -> Result<(u64, u64)> {
        permissions::count_out_tree_for_permissions_dirs(&self.conn())
    }

    /// See [`permissions::count_canonical_files_for_permissions`].
    pub fn count_canonical_files_for_permissions(&self) -> Result<(u64, u64)> {
        permissions::count_canonical_files_for_permissions(&self.conn())
    }

    pub fn apply_permissions_flags_to_files(&self) -> Result<(u64, u64)> {
        permissions::apply_permissions_flags_to_files(&self.conn())
    }

    pub fn ingest_apply_permission_out_tree_results(&self, res: &Vec<(OutTreeId, bool)>) -> Result<u64> {
        permissions::ingest_apply_permission_out_tree_results(&mut self.conn_mut(), res)
    }

    pub fn ingest_apply_permission_file_results(&self, res: &Vec<(FileId, bool)>) -> Result<u64> {
        permissions::ingest_apply_permission_file_results(&mut self.conn_mut(), res)
    }
    // --- errors (persistent error log) ---

    pub fn insert_errors(&self, drafts: &[RecordDraft]) -> Result<u64> {
        errors::insert_errors(&mut *self.conn_mut(), drafts)
    }

    pub fn get_record_by_id(&self, id: i64) -> Result<Option<ErrorRecord>> {
        errors::get_record_by_id(&self.conn(), id)
    }

    pub fn get_records_by_file_id(&self, file_id: FileId) -> Result<Vec<ErrorRecord>> {
        errors::get_records_by_file_id(&self.conn(), file_id)
    }

    pub fn get_records_by_out_tree_id(&self, out_tree_id: OutTreeId) -> Result<Vec<ErrorRecord>> {
        errors::get_records_by_out_tree_id(&self.conn(), out_tree_id)
    }

    pub fn list_records(
        &self,
        scope: flags::ErrorScope,
        reemit: Option<(bool, i64)>,
        last_id: i64,
        batch_size: u64)
        -> Result<Vec<ErrorRecord>> {
        errors::list_records(&self.conn(), scope, reemit, last_id, batch_size)
    }

    pub fn count_records(
        &self,
        scope: flags::ErrorScope,
        reemit: Option<(bool, i64)>,
    ) -> Result<u64> {
        errors::count_records(&self.conn(), scope, reemit)
    }

    pub fn count_errors(&self, phase: Option<&str>) -> Result<u64> {
        errors::count_errors(&self.conn(), phase)
    }

    // --- integrity checks across the database

    pub fn count_missing_dev_inode(&self) -> Result<u64> {
        integrity::count_missing_dev_inode(&self.conn())
    }

    pub fn list_missing_dev_inode<R: SqlFileRow>(&self, last_id: &FileId, batch_size: u64)
        -> Result<Vec<R>> {
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
    // --- cross-phase invariant battery (`plans/025-sanity-checks.md`) ---

    /// Run the archive battery after a completed phase. `progress` fires once
    /// per completed check (integrity counter bar); pass `None` to skip.
    pub fn integrity_check_after_archive(
        &self, gate: &ArchiveGate, phase: PipelinePhase,
        progress: Option<&dyn Fn()>) -> Result<()> {
        integrity::check_after_archive_phase(&self.conn(), gate, phase, progress)
    }

    /// Run the archive (config-free subset) + extract battery. `progress` fires
    /// once per completed check (integrity counter bar); pass `None` to skip.
    pub fn integrity_check_after_extract(
        &self, config: &ExtractConfig, phase: ExtractPipelinePhase,
        progress: Option<&dyn Fn()>) -> Result<()> {
        integrity::check_after_extract_phase(&self.conn(), config, phase, progress)
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
/// [`crate::common::defaults::DEFAULT_AUTO_FLUSH_LIMIT`] a flush is attempted so bursts of
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
    /// Recorder tied to an already-open database. `flush_limit` overrides the
    /// auto-flush threshold; `None` uses the default from `defaults.rs`.
    pub fn new(db: &'a Database, enabled: bool, flush_limit: Option<u64>) -> Self {
        Self {
            db: Some(db),
            buf: Vec::new(),
            enabled,
            auto_flush: true,
            flush_limit: flush_limit
                .unwrap_or(DEFAULT_AUTO_FLUSH_LIMIT),
        }
    }

    /// Speculative recorder: buffers errors without a database. Attach one later
    /// with [`bind`]; until then a [`flush`] only reports that it could not store.
    /// `flush_limit` overrides the auto-flush threshold; `None` uses the default.
    pub fn speculative(enabled: bool, flush_limit: Option<u64>) -> Self {
        Self {
            db: None,
            buf: Vec::new(),
            enabled,
            auto_flush: true,
            flush_limit: flush_limit
                .unwrap_or(DEFAULT_AUTO_FLUSH_LIMIT),
        }
    }

    /// Return if the function contains a database, to enable smart flushing.
    pub fn is_speculative(&self) -> bool {
        self.db.is_none()
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
    /// Useful once a fresh-from-disk database is attached to the process and its
    /// stored config carries the authoritative limit (e.g. extract/resume).
    pub fn set_flush_limit(&mut self, limit: u64) {
        self.flush_limit = limit;
    }

    pub fn record(
        &mut self,
        file_id: Option<FileId>,
        out_tree_id: Option<OutTreeId>,
        phase: ErrorPhase,
        error: FileStatError,
        flags: flags::ErrorFlags) {

        if !self.enabled {
            return;
        }

        // INFO: auto_flush in push!
        self.push(RecordDraft {
            file_id,
            out_tree_id,
            phase,
            error,
            flags,
        });
    }

    /// Session-scoped error: neither a file nor an out_tree row.
    ///
    /// The `SessionError` flag is always set, regardless of `flags`.
    pub fn record_session(
        &mut self,
        phase: ErrorPhase,
        error: FileStatError,
        flags: flags::ErrorFlags) {

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
        error: FileStatError,
        flags: flags::ErrorFlags,
    ) {
        self.record(Some(file_id), None, phase, error, flags);
    }

    /// Out-tree-scoped error.
    pub fn record_out_tree(
        &mut self,
        out_tree_id: OutTreeId,
        phase: ErrorPhase,
        error: FileStatError,
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

    /// Only flush, if a database is present. Otherwise, hold on to the entries.
    pub fn try_flush(&mut self) -> Result<u64> {
        if self.is_speculative() {
            return Ok(0);
        };
        self.flush()
    }

    // TODO: Can we assume that try_flush is called and if this function is called, all messages
    //  are emitted to std::out at i.g. info ot err to be able to suppress this.
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

#[cfg(test)]
mod tests {
    use super::*;

    fn version_of(path: &std::path::Path) -> Option<String> {
        let conn = Connection::open(path).expect("open raw");
        meta::get_db_version(&conn).expect("read version")
    }

    #[test]
    fn open_stamps_db_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sqlite");
        let db = Database::open(&path).unwrap();
        assert_eq!(
            db.get_db_version().unwrap().as_deref(),
            Some(crate::common::DB_VERSION)
        );
    }

    #[test]
    fn open_adopts_versionless_db_and_keeps_stored_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.sqlite");
        // A work-db created without the `db_version` stamp (pre-version) is
        // adopted silently on first open...
        {
            let conn = Connection::open(&path).unwrap();
            schema::initialize(&conn).unwrap();
        }
        {
            let db = Database::open(&path).unwrap();
            assert_eq!(
                db.get_db_version().unwrap().as_deref(),
                Some(crate::common::DB_VERSION)
            );
        }
        // ...and a stored version is never overwritten on reopen.
        {
            let conn = Connection::open(&path).unwrap();
            meta::set_db_version(&conn, "9.9.9").unwrap();
        }
        {
            let db = Database::open(&path).unwrap();
            assert_eq!(db.get_db_version().unwrap().as_deref(), Some("9.9.9"));
            assert_eq!(version_of(&path).as_deref(), Some("9.9.9"));
        }
    }
}
