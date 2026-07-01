use rusqlite::{params, Connection, OptionalExtension};

use crate::config::{ExtractPipelinePhase, ExtractRuntimeState};
use crate::db::inventory;
use crate::db::types::{FileId, FilePhase, FileRecord};
use crate::error::Result;

/// Tar member observed during scan: mark the matching canonical file row (by tar_path).
pub fn record_tar_seen(conn: &Connection, tar_path: &str, size: u64) -> Result<Option<FileId>> {
    let Some(record) = inventory::get_file_by_tar_path(conn, tar_path)? else {
        return Ok(None);
    };
    if record.size != size {
        return Ok(None);
    }
    inventory::mark_phase(conn, record.id, FilePhase::TarSeen)?;
    Ok(Some(record.id))
}

pub fn get_file_by_tar_path(conn: &Connection, tar_path: &str) -> Result<Option<FileRecord>> {
    inventory::get_file_by_tar_path(conn, tar_path)
}

/// After ingesting snapshot.sqlite: files still marked archived become unarchived.
pub fn apply_snapshot_archived(conn: &Connection) -> Result<u64> {
    let updated = conn.execute(
        "UPDATE files SET phase = 'unarchived' WHERE phase = 'archived'",
        [],
    )?;
    Ok(updated as u64)
}

pub fn load_extract_runtime_state(conn: &Connection) -> Result<Option<ExtractRuntimeState>> {
    let phase = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'extract_phase'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;

    let Some(phase_raw) = phase else {
        return Ok(None);
    };

    let snapshots_ingested: u32 = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'extract_snapshots_ingested'",
            [],
            |row| row.get::<_, String>(0),
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
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}
