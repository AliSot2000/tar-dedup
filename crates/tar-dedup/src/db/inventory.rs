use chrono::{DateTime, Utc};
use rusqlite::{named_params, Connection, OptionalExtension};

use crate::config::{PipelinePhase, RuntimeState};
use crate::db::types::{FileId, FilePhase, FileRecord, NewFileRecord};
use crate::error::Result;

const FILES_SELECT: &str =
    "id, rel_path, size, sha1, mtime, atime, uid, gid, mode, canonical_id, tar_path, snapshot_archived";

pub fn insert_file(conn: &Connection, record: &NewFileRecord) -> Result<bool> {
    let changed = conn.execute(
        "INSERT OR IGNORE INTO files (rel_path, size, mtime, atime, uid, gid, mode, phase)
         VALUES (:rel_path, :size, :mtime, :atime, :uid, :gid, :mode, 'inventoried')",
        named_params! {
            ":rel_path": record.rel_path.to_string_lossy(),
            ":size": record.size,
            ":mtime": record.mtime.as_ref().map(|t| t.to_rfc3339()),
            ":atime": record.atime.as_ref().map(|t| t.to_rfc3339()),
            ":uid": record.uid,
            ":gid": record.gid,
            ":mode": record.mode,
        },
    )?;
    Ok(changed > 0)
}

pub fn count_files(conn: &Connection) -> Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files",
        [],
        |row| row.get("count"),
    )?;
    Ok(count as u64)
}

pub fn list_files_in_phase(conn: &Connection, phase: FilePhase) -> Result<Vec<FileRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {FILES_SELECT} FROM files WHERE phase = :phase ORDER BY id"
    ))?;

    let rows = stmt.query_map(
        named_params! { ":phase": phase.as_str() },
        map_file_record,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn get_file(conn: &Connection, file_id: FileId) -> Result<Option<FileRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {FILES_SELECT} FROM files WHERE id = :id"
    ))?;
    let mut rows = stmt.query(named_params! { ":id": file_id.0 })?;
    if let Some(row) = rows.next()? {
        return Ok(Some(map_file_record(row)?));
    }
    Ok(None)
}

pub fn get_file_by_tar_path(conn: &Connection, tar_path: &str) -> Result<Option<FileRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {FILES_SELECT} FROM files WHERE tar_path = :tar_path LIMIT 1"
    ))?;
    let mut rows = stmt.query(named_params! { ":tar_path": tar_path })?;
    if let Some(row) = rows.next()? {
        return Ok(Some(map_file_record(row)?));
    }
    Ok(None)
}

pub fn set_tar_path(conn: &Connection, file_id: FileId, tar_path: &str) -> Result<()> {
    conn.execute(
        "UPDATE files SET tar_path = :tar_path WHERE id = :id",
        named_params! {
            ":tar_path": tar_path,
            ":id": file_id.0,
        },
    )?;
    Ok(())
}

pub fn mark_phase(conn: &Connection, file_id: FileId, phase: FilePhase) -> Result<()> {
    conn.execute(
        "UPDATE files SET phase = :phase WHERE id = :id",
        named_params! {
            ":phase": phase.as_str(),
            ":id": file_id.0,
        },
    )?;
    Ok(())
}

pub(crate) fn map_file_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    let sha1_blob: Option<Vec<u8>> = row.get("sha1")?;
    let sha1 = sha1_blob
        .and_then(|b| b.try_into().ok())
        .map(|arr: [u8; 20]| arr);

    Ok(FileRecord {
        id: FileId(row.get("id")?),
        rel_path: row.get::<_, String>("rel_path")?.into(),
        size: row.get::<_, i64>("size")? as u64,
        sha1,
        mtime: optional_rfc3339(row, "mtime")?,
        atime: optional_rfc3339(row, "atime")?,
        uid: row.get::<_, Option<i64>>("uid")?.map(|v| v as u32),
        gid: row.get::<_, Option<i64>>("gid")?.map(|v| v as u32),
        mode: row.get::<_, Option<i64>>("mode")?.map(|v| v as u32),
        canonical_id: row.get::<_, Option<i64>>("canonical_id")?.map(FileId),
        tar_path: row.get("tar_path")?,
        snapshot_archived: row.get::<_, i64>("snapshot_archived")? != 0,
    })
}

fn optional_rfc3339(
    row: &rusqlite::Row<'_>,
    column: &str,
) -> rusqlite::Result<Option<DateTime<Utc>>> {
    let raw: Option<String> = row.get(column)?;
    match raw {
        None => Ok(None),
        Some(s) => DateTime::parse_from_rfc3339(&s)
            .map(|dt| Some(dt.with_timezone(&Utc)))
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            }),
    }
}

pub fn load_runtime_state(conn: &Connection) -> Result<Option<RuntimeState>> {
    let phase = conn
        .query_row(
            "SELECT value FROM meta WHERE key = :key",
            named_params! { ":key": "phase" },
            |row| row.get::<_, String>("value"),
        )
        .optional()?;

    let Some(phase_raw) = phase else {
        return Ok(None);
    };

    let max_workers: usize = conn
        .query_row(
            "SELECT value FROM meta WHERE key = :key",
            named_params! { ":key": "max_workers" },
            |row| row.get::<_, String>("value"),
        )?
        .parse()
        .map_err(|_| {
            crate::error::Error::Config("invalid max_workers in meta".into())
        })?;

    let snapshot_taken_at = conn
        .query_row(
            "SELECT value FROM meta WHERE key = :key",
            named_params! { ":key": "snapshot_taken_at" },
            |row| row.get::<_, String>("value"),
        )?
        .parse()
        .map_err(|_| {
            crate::error::Error::Config("invalid snapshot_taken_at in meta".into())
        })?;

    Ok(Some(RuntimeState {
        snapshot_taken_at,
        phase: PipelinePhase::parse(&phase_raw)?,
        max_workers,
    }))
}

pub fn save_runtime_state(conn: &Connection, state: &RuntimeState) -> Result<()> {
    upsert_meta(conn, "phase", state.phase.as_str())?;
    upsert_meta(conn, "snapshot_taken_at", &state.snapshot_taken_at.to_rfc3339())?;
    upsert_meta(conn, "max_workers", &state.max_workers.to_string())?;
    Ok(())
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
