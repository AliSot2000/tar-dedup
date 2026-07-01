use rusqlite::{params, Connection, OptionalExtension};

use crate::config::{PipelinePhase, RuntimeState};
use crate::db::types::{FileId, FilePhase, FileRecord, NewFileRecord};
use crate::error::Result;

pub fn insert_file(conn: &Connection, record: &NewFileRecord) -> Result<bool> {
    let changed = conn.execute(
        "INSERT OR IGNORE INTO files (rel_path, size, mtime, atime, uid, gid, mode, phase)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'inventoried')",
        params![
            record.rel_path.to_string_lossy(),
            record.size,
            record.mtime,
            record.atime,
            record.uid,
            record.gid,
            record.mode,
        ],
    )?;
    Ok(changed > 0)
}

pub fn count_files(conn: &Connection) -> Result<u64> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))?;
    Ok(count as u64)
}

pub fn list_files_in_phase(conn: &Connection, phase: FilePhase) -> Result<Vec<FileRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, rel_path, size, sha1, mtime, atime, uid, gid, mode, canonical_id, tar_path
         FROM files
         WHERE phase = ?1
         ORDER BY id",
    )?;

    let rows = stmt.query_map([phase.as_str()], map_file_record)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn get_file(conn: &Connection, file_id: FileId) -> Result<Option<FileRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, rel_path, size, sha1, mtime, atime, uid, gid, mode, canonical_id, tar_path
         FROM files WHERE id = ?1",
    )?;
    let mut rows = stmt.query([file_id.0])?;
    if let Some(row) = rows.next()? {
        return Ok(Some(map_file_record(row)?));
    }
    Ok(None)
}

pub fn get_file_by_tar_path(conn: &Connection, tar_path: &str) -> Result<Option<FileRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, rel_path, size, sha1, mtime, atime, uid, gid, mode, canonical_id, tar_path
         FROM files
         WHERE tar_path = ?1
         LIMIT 1",
    )?;
    let mut rows = stmt.query([tar_path])?;
    if let Some(row) = rows.next()? {
        return Ok(Some(map_file_record(row)?));
    }
    Ok(None)
}

pub fn set_tar_path(conn: &Connection, file_id: FileId, tar_path: &str) -> Result<()> {
    conn.execute(
        "UPDATE files SET tar_path = ?1 WHERE id = ?2",
        params![tar_path, file_id.0],
    )?;
    Ok(())
}

pub fn mark_phase(conn: &Connection, file_id: FileId, phase: FilePhase) -> Result<()> {
    conn.execute(
        "UPDATE files SET phase = ?1 WHERE id = ?2",
        params![phase.as_str(), file_id.0],
    )?;
    Ok(())
}

pub(crate) fn map_file_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    let sha1_blob: Option<Vec<u8>> = row.get(3)?;
    let sha1 = sha1_blob
        .and_then(|b| b.try_into().ok())
        .map(|arr: [u8; 20]| arr);

    Ok(FileRecord {
        id: FileId(row.get(0)?),
        rel_path: row.get::<_, String>(1)?.into(),
        size: row.get::<_, i64>(2)? as u64,
        sha1,
        mtime: row.get(4)?,
        atime: row.get(5)?,
        uid: row.get::<_, Option<i64>>(6)?.map(|v| v as u32),
        gid: row.get::<_, Option<i64>>(7)?.map(|v| v as u32),
        mode: row.get::<_, Option<i64>>(8)?.map(|v| v as u32),
        canonical_id: row.get::<_, Option<i64>>(9)?.map(FileId),
        tar_path: row.get(10)?,
    })
}

pub fn load_runtime_state(conn: &Connection) -> Result<Option<RuntimeState>> {
    let phase = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'phase'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?;

    let Some(phase_raw) = phase else {
        return Ok(None);
    };

    let max_workers: usize = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'max_workers'",
            [],
            |row| row.get::<_, String>(0),
        )?
        .parse()
        .map_err(|_| {
            crate::error::Error::Config("invalid max_workers in meta".into())
        })?;

    let snapshot_taken_at = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'snapshot_taken_at'",
            [],
            |row| row.get::<_, String>(0),
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
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}
