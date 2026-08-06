use std::fs;
use std::path::Path;

use rusqlite::{named_params, Connection};

use crate::config::ExtractRuntimeState;
use crate::db::common::SqlFileRow;
use crate::db::meta;
use crate::db::content_id::parse_content_id;
use crate::db::flags::FileFlag;
use crate::db::types::FileId;
use crate::error::{Error, Result};

/// Cumulative + per-pass extract scan observations persisted in `meta`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtractScanState {
    pub saw_manifest_db: bool,
    pub saw_any_members: bool,
    /// Set only when the tar entry iterator is exhausted.
    pub scan_complete: bool,
    /// Index of the last tar member that was fully processed; `None` while no member
    /// has been. A resumed pass restarts at the following index.
    pub last_member_index: Option<u64>,
    pub from_footer: bool,
    /// Cumulative `snapshot.sqlite` members ingested (persisted).
    pub snapshots_ingested: u32,
}

/// Copy an embedded catalog into the extract work DB.
pub fn install_initial_manifest(snapshot_path: &Path, db_path: &Path) -> Result<()> {
    if db_path.is_file() {
        std::fs::remove_file(db_path).map_err(|e| Error::io(db_path, e))?;
    }
    std::fs::copy(snapshot_path, db_path).map_err(|e| Error::io(db_path, e))?;
    Ok(())
}

/// Normalize a freshly installed catalog so stream handling is provenance-agnostic.
/// Clears [`FileFlag::FileExtracted`] and forces candidate rows to `archived`.
/// Must not run on a resumed work DB.
pub fn normalize_installed_catalog(conn: &Connection) -> Result<()> {
    let bit = FileFlag::FileExtracted.mask_i64();
    conn.execute(
        "UPDATE files SET flags = flags & ~:bit",
        named_params! { ":bit": bit },
    )?;
    // Candidate regular-file rows (and anything still mid-pipeline) → archived.
    conn.execute(
        "UPDATE files
         SET phase = 'archived'
         WHERE phase IN (
             'inventoried', 'hashed', 'filtered', 'deduped', 'sparsified',
             'staged', 'archived'
         )",
        [],
    )?;
    meta::clear_archive_meta(conn)?;
    Ok(())
}

/// Snapshot confirmation: promote `archived` → `unarchived` for paths the snapshot
/// lists as archived whose payload has been extracted (canonical carries
/// [`FileFlag::FileExtracted`]), fanning phase out over `canonical_id`.
pub fn apply_snapshot_promote_unarchived(
    conn: &Connection,
    snapshot_path: &Path,
) -> Result<u64> {
    let path = snapshot_path.to_string_lossy();
    let bit = FileFlag::FileExtracted.mask_i64();
    conn.execute(
        "ATTACH DATABASE :path AS snap",
        named_params! { ":path": path.as_ref() },
    )?;
    let promoted = conn.execute(
        "UPDATE files
         SET phase = 'unarchived'
         WHERE phase = 'archived'
           AND rel_path IN (SELECT rel_path FROM snap.files WHERE phase = 'archived')
           AND (
                 (flags & :bit) != 0
              OR canonical_id IN (
                     SELECT id FROM files WHERE (flags & :bit) != 0
                 )
           )",
        named_params! { ":bit": bit },
    )?;
    // Descendants of already-unarchived canonicals.
    conn.execute(
        "UPDATE files
         SET phase = 'unarchived'
         WHERE phase = 'archived'
           AND canonical_id IS NOT NULL
           AND canonical_id IN (SELECT id FROM files WHERE phase = 'unarchived')",
        [],
    )?;
    conn.execute("DETACH DATABASE snap", [])?;
    Ok(promoted as u64)
}

/// Payload landed in extract cache for canonical `file_id` → set [`FileFlag::FileExtracted`]
/// on the canonical row only. Phase stays `archived` until snapshot confirmation
/// (or end-of-scan salvage under footer / force-scan).
pub fn mark_file_extracted(conn: &Connection, file_id: FileId) -> Result<()> {
    let bit = FileFlag::FileExtracted.mask_i64();
    conn.execute(
        "UPDATE files
         SET flags = flags | :bit
         WHERE id = :id",
        named_params! {
            ":bit": bit,
            ":id": file_id.0,
        },
    )?;
    Ok(())
}

/// End-of-scan salvage: promote every `FileExtracted` canonical (and dependents)
/// that is still `archived` → `unarchived`. Used when `from_footer || force_scan`.
pub fn promote_extracted_to_unarchived(conn: &Connection) -> Result<u64> {
    let bit = FileFlag::FileExtracted.mask_i64();
    let n = conn.execute(
        "UPDATE files
         SET phase = 'unarchived'
         WHERE phase = 'archived'
           AND (
                 (flags & :bit) != 0
              OR canonical_id IN (
                     SELECT id FROM files WHERE (flags & :bit) != 0
                 )
           )",
        named_params! { ":bit": bit },
    )?;
    Ok(n as u64)
}

/// Mark every content-id named payload sitting in the extract cache as extracted.
/// Catches members that were unpacked but not flagged (interrupt between the two).
/// Promotion stays with snapshot confirmation / [`promote_extracted_to_unarchived`].
pub fn flush_cached_payloads(conn: &Connection, cache_dir: &Path) -> Result<u64> {
    let mut marked = 0u64;
    if cache_dir.is_dir() {
        for entry in fs::read_dir(cache_dir).map_err(|e| Error::io(cache_dir, e))? {
            let entry = entry.map_err(|e| Error::io(cache_dir, e))?;
            let ft = entry.file_type().map_err(|e| Error::io(&entry.path(), e))?;
            if !ft.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Ok((_, _, file_id, _)) = parse_content_id(name) else {
                continue;
            };
            mark_file_extracted(conn, file_id)?;
            marked += 1;
        }
    }
    Ok(marked)
}

pub fn list_files_to_restore<R: SqlFileRow>(conn: &Connection) -> Result<Vec<R>> {
    let cols = R::sql_columns();
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols} FROM files WHERE phase = 'rehashed' ORDER BY id"
    ))?;
    let rows = stmt.query_map([], R::from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Promote every `unarchived` row to `rehashed` without verifying payloads.
pub fn skip_rehash(conn: &Connection) -> Result<u64> {
    // INFO: Technically, there should not be any file that is not 'unarchived' or 'rehashed'
    let n = conn.execute(  
        "UPDATE files SET phase = 'rehashed' WHERE phase = 'unarchived'",
        [],
    )?;
    Ok(n as u64)
}

/// Canonical rows with `AppendedPath` but without `FileExtracted`.
pub fn count_missing_payloads(conn: &Connection) -> Result<u64> {
    let appended = FileFlag::AppendedPath.mask_i64();
    let extracted = FileFlag::FileExtracted.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE canonical_id = id
           AND (flags & :appended) != 0
           AND (flags & :extracted) = 0",
        named_params! {
            ":appended": appended,
            ":extracted": extracted,
        },
        |row| row.get("count"),
    )?;
    Ok(count as u64)
}

/// Canonical rows carrying `FileExtracted`.
pub fn count_extracted_canonical(conn: &Connection) -> Result<u64> {
    let bit = FileFlag::FileExtracted.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE canonical_id = id AND (flags & :bit) != 0",
        named_params! { ":bit": bit },
        |row| row.get("count"),
    )?;
    Ok(count as u64)
}

/// All rows in canonical groups that have `FileExtracted` on the canonical.
pub fn count_extracted_paths(conn: &Connection) -> Result<u64> {
    let bit = FileFlag::FileExtracted.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE (flags & :bit) != 0
            OR canonical_id IN (
                   SELECT id FROM files WHERE (flags & :bit) != 0
               )",
        named_params! { ":bit": bit },
        |row| row.get("count"),
    )?;
    Ok(count as u64)
}

/// `(ftype_label, count)` for rows that lack `AppendedPath` (NULL ftype → `"null"`).
pub fn count_non_appended_by_ftype(conn: &Connection) -> Result<Vec<(String, u64)>> {
    let bit = FileFlag::AppendedPath.mask_i64();
    let mut stmt = conn.prepare(
        "SELECT COALESCE(ftype, 'null') AS ft, COUNT(*) AS count
         FROM files
         WHERE (flags & :bit) = 0
         GROUP BY ftype
         ORDER BY ft",
    )?;
    let rows = stmt.query_map(named_params! { ":bit": bit }, |row| {
        Ok((row.get::<_, String>("ft")?, row.get::<_, i64>("count")? as u64))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn load_extract_scan_state(conn: &Connection) -> Result<ExtractScanState> {
    Ok(ExtractScanState {
        saw_manifest_db: meta::get_scan_tar_saw_manifest_db(conn)?.unwrap_or(false),
        saw_any_members: meta::get_scan_tar_saw_any_members(conn)?.unwrap_or(false),
        scan_complete: meta::get_scan_tar_complete(conn)?.unwrap_or(false),
        last_member_index: meta::get_scan_tar_last_member_index(conn)?,
        from_footer: meta::get_scan_tar_from_footer(conn)?.unwrap_or(false),
        snapshots_ingested: meta::get_extract_snapshots_ingested(conn)?.unwrap_or(0),
    })
}

pub fn save_extract_scan_state(conn: &Connection, state: &ExtractScanState) -> Result<()> {
    meta::with_meta_txn(conn, |conn| {
        meta::set_scan_tar_saw_manifest_db(conn, state.saw_manifest_db)?;
        meta::set_scan_tar_saw_any_members(conn, state.saw_any_members)?;
        meta::set_scan_tar_complete(conn, state.scan_complete)?;
        match state.last_member_index {
            Some(index) => meta::set_scan_tar_last_member_index(conn, index)?,
            None => meta::delete_scan_tar_last_member_index(conn)?,
        }
        meta::set_scan_tar_from_footer(conn, state.from_footer)?;
        meta::set_extract_snapshots_ingested(conn, state.snapshots_ingested)?;
        Ok(())
    })
}

pub fn init_extract_runtime_state(conn: &Connection) -> Result<()> {
    if load_extract_runtime_state(conn)?.is_none() {
        save_extract_runtime_state(conn, &ExtractRuntimeState::new())?;
    }
    Ok(())
}

pub fn load_extract_runtime_state(conn: &Connection) -> Result<Option<ExtractRuntimeState>> {
    let Some(phase) = meta::get_extract_phase(conn)? else {
        return Ok(None);
    };
    let snapshots_ingested = meta::get_extract_snapshots_ingested(conn)?.unwrap_or(0);
    Ok(Some(ExtractRuntimeState {
        phase,
        snapshots_ingested,
    }))
}

pub fn save_extract_runtime_state(conn: &Connection, state: &ExtractRuntimeState) -> Result<()> {
    meta::with_meta_txn(conn, |conn| {
        meta::set_extract_phase(conn, state.phase)?;
        meta::set_extract_snapshots_ingested(conn, state.snapshots_ingested)?;
        Ok(())
    })
}

pub fn record_snapshot_ingested(conn: &Connection) -> Result<u32> {
    let mut scan = load_extract_scan_state(conn)?;
    scan.snapshots_ingested = scan.snapshots_ingested.saturating_add(1);
    save_extract_scan_state(conn, &scan)?;
    // Keep ExtractRuntimeState in sync when present.
    if let Some(mut runtime) = load_extract_runtime_state(conn)? {
        runtime.snapshots_ingested = scan.snapshots_ingested;
        save_extract_runtime_state(conn, &runtime)?;
    }
    Ok(scan.snapshots_ingested)
}
