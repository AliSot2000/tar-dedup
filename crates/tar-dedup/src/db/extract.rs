use std::path::Path;

use rusqlite::{named_params, Connection, OptionalExtension};

use crate::config::{ExtractPipelinePhase, ExtractRuntimeState};
use crate::db::inventory;
use crate::db::types::FileRecord;
use crate::error::Result;

/// Copy the first embedded snapshot into the extract work DB (initial manifest).
pub fn install_initial_manifest(snapshot_path: &Path, db_path: &Path) -> Result<()> {
    if db_path.is_file() {
        std::fs::remove_file(db_path).map_err(|e| crate::error::Error::io(db_path, e))?;
    }
    std::fs::copy(snapshot_path, db_path).map_err(|e| crate::error::Error::io(db_path, e))?;
    Ok(())
}

/// Rows listed as `archived` in an ingested snapshot → `snapshot_archived = 1` (catalog confirmation).
pub fn apply_snapshot_archived_flags(conn: &Connection, snapshot_path: &Path) -> Result<u64> {
    let path = snapshot_path.to_string_lossy();
    conn.execute(
        "ATTACH DATABASE :path AS snap",
        named_params! { ":path": path.as_ref() },
    )?;
    let flagged = conn.execute(
        "UPDATE files SET snapshot_archived = 1
         WHERE rel_path IN (SELECT rel_path FROM snap.files WHERE phase = 'archived')",
        [],
    )?;
    conn.execute(
        "UPDATE files SET snapshot_archived = 1
         WHERE snapshot_archived = 0
           AND canonical_id IN (SELECT id FROM files WHERE snapshot_archived = 1)",
        [],
    )?;
    conn.execute("DETACH DATABASE snap", [])?;
    Ok(flagged as u64)
}

/// Payload landed in extract cache → ready to place (`snapshot_archived` unchanged).
pub fn promote_cached_tar_member(conn: &Connection, tar_path: &str) -> Result<()> {
    conn.execute(
        "UPDATE files SET phase = 'unarchived' WHERE tar_path = :tar_path",
        named_params! { ":tar_path": tar_path },
    )?;
    conn.execute(
        "UPDATE files SET phase = 'unarchived'
         WHERE tar_path IS NULL
           AND canonical_id = (SELECT id FROM files WHERE tar_path = :tar_path LIMIT 1)",
        named_params! { ":tar_path": tar_path },
    )?;
    Ok(())
}

pub fn list_files_to_restore(conn: &Connection) -> Result<Vec<FileRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, rel_path, size, sha1, mtime, atime, uid, gid, mode, canonical_id, tar_path, snapshot_archived
         FROM files
         WHERE phase = 'unarchived'
         ORDER BY id",
    )?;
    let rows = stmt.query_map([], inventory::map_file_record)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn count_unconfirmed_restored(conn: &Connection) -> Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE phase IN ('unarchived', 'at_destination', 'link_at_destination')
           AND snapshot_archived = 0",
        [],
        |row| row.get("count"),
    )?;
    Ok(count as u64)
}

pub fn tar_member_path(conn: &Connection, record: &FileRecord) -> Result<String> {
    if let Some(ref path) = record.tar_path {
        return Ok(path.clone());
    }
    let canonical_id = record.canonical_id.unwrap_or(record.id);
    let canonical = inventory::get_file(conn, canonical_id)?.ok_or_else(|| {
        crate::error::Error::Config(format!(
            "missing canonical file id {} for {}",
            canonical_id.0,
            record.rel_path.display()
        ))
    })?;
    canonical.tar_path.ok_or_else(|| {
        crate::error::Error::Config(format!(
            "no tar member for {}",
            record.rel_path.display()
        ))
    })
}

pub fn init_extract_runtime_state(conn: &Connection) -> Result<()> {
    if load_extract_runtime_state(conn)?.is_none() {
        save_extract_runtime_state(conn, &ExtractRuntimeState::new())?;
    }
    Ok(())
}

pub fn load_extract_runtime_state(conn: &Connection) -> Result<Option<ExtractRuntimeState>> {
    let phase = conn
        .query_row(
            "SELECT value FROM meta WHERE key = :key",
            named_params! { ":key": "extract_phase" },
            |row| row.get::<_, String>("value"),
        )
        .optional()?;

    let Some(phase_raw) = phase else {
        return Ok(None);
    };

    let snapshots_ingested: u32 = conn
        .query_row(
            "SELECT value FROM meta WHERE key = :key",
            named_params! { ":key": "extract_snapshots_ingested" },
            |row| row.get::<_, String>("value"),
        )?
        .parse()
        .map_err(|_| {
            crate::error::Error::Config("invalid extract_snapshots_ingested in meta".into())
        })?;

    Ok(Some(ExtractRuntimeState {
        phase: ExtractPipelinePhase::parse(&phase_raw)?,
        snapshots_ingested,
    }))
}

pub fn save_extract_runtime_state(conn: &Connection, state: &ExtractRuntimeState) -> Result<()> {
    upsert_meta(conn, "extract_phase", state.phase.as_str())?;
    upsert_meta(
        conn,
        "extract_snapshots_ingested",
        &state.snapshots_ingested.to_string(),
    )?;
    Ok(())
}

pub fn record_snapshot_ingested(conn: &Connection) -> Result<u32> {
    let state = load_extract_runtime_state(conn)?.ok_or_else(|| {
        crate::error::Error::Config("extract runtime state not initialized".into())
    })?;
    let next = ExtractRuntimeState {
        snapshots_ingested: state.snapshots_ingested + 1,
        ..state
    };
    save_extract_runtime_state(conn, &next)?;
    Ok(next.snapshots_ingested)
}

fn upsert_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (:key, :value)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        named_params! {
            ":key": key,
            ":value": value,
        },
    )?;
    Ok(())
}
