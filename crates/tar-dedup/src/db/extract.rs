use std::path::Path;

use rusqlite::{named_params, Connection, OptionalExtension};

use crate::config::{ExtractPipelinePhase, ExtractRuntimeState};
use crate::db::common::{upsert_meta, SqlFileRow};
use crate::db::flags::FileFlag;
use crate::error::Result;

/// Copy the first embedded snapshot into the extract work DB (initial manifest).
pub fn install_initial_manifest(snapshot_path: &Path, db_path: &Path) -> Result<()> {
    if db_path.is_file() {
        std::fs::remove_file(db_path).map_err(|e| crate::error::Error::io(db_path, e))?;
    }
    std::fs::copy(snapshot_path, db_path).map_err(|e| crate::error::Error::io(db_path, e))?;
    Ok(())
}

/// Rows listed as `archived` in an ingested snapshot → set `SnapshotArchived` flag.
pub fn apply_snapshot_archived_flags(conn: &Connection, snapshot_path: &Path) -> Result<u64> {
    let path = snapshot_path.to_string_lossy();
    let bit = FileFlag::SnapshotArchived.mask_i64();
    conn.execute(
        "ATTACH DATABASE :path AS snap",
        named_params! { ":path": path.as_ref() },
    )?;
    let flagged = conn.execute(
        "UPDATE files SET flags = flags | :bit
         WHERE rel_path IN (SELECT rel_path FROM snap.files WHERE phase = 'archived')",
        named_params! { ":bit": bit },
    )?;
    conn.execute(
        "UPDATE files SET flags = flags | :bit
         WHERE (flags & :bit) = 0
           AND canonical_id IN (SELECT id FROM files WHERE (flags & :bit) != 0)",
        named_params! { ":bit": bit },
    )?;
    conn.execute("DETACH DATABASE snap", [])?;
    Ok(flagged as u64)
}

/// Payload landed in extract cache → ready to place (`SnapshotArchived` unchanged).
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

pub fn list_files_to_restore<R: SqlFileRow>(conn: &Connection) -> Result<Vec<R>> {
    let cols = R::sql_columns();
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols} FROM files WHERE phase = 'unarchived' ORDER BY id"
    ))?;
    let rows = stmt.query_map([], R::from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn count_unconfirmed_restored(conn: &Connection) -> Result<u64> {
    let bit = FileFlag::SnapshotArchived.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE phase IN ('unarchived', 'at_destination', 'link_at_destination')
           AND (flags & :bit) = 0",
        named_params! { ":bit": bit },
        |row| row.get("count"),
    )?;
    Ok(count as u64)
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
