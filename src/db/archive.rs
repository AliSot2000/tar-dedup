use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};

use crate::db::types::{ArchiveSession, FileId};
use crate::error::Result;

pub fn begin_session(conn: &Connection, stream_index: i64, archive_offset: u64) -> Result<i64> {
    conn.execute(
        "INSERT INTO archive_sessions (stream_index, archive_offset, started_at) VALUES (?1, ?2, ?3)",
        params![stream_index, archive_offset as i64, Utc::now().to_rfc3339()],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn finalize_session(conn: &Connection, session_id: i64, bytes_in: u64, bytes_out: u64) -> Result<()> {
    conn.execute(
        "UPDATE archive_sessions
         SET finalized = 1, bytes_in = ?1, bytes_out = ?2, finished_at = ?3
         WHERE id = ?4",
        params![bytes_in as i64, bytes_out as i64, Utc::now().to_rfc3339(), session_id],
    )?;
    Ok(())
}

pub fn next_stream_index(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(stream_index), -1) + 1 FROM archive_sessions",
        [],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

pub fn queue_entry(conn: &Connection, file_id: FileId, session_id: i64, tar_path: &str) -> Result<i64> {
    conn.execute(
        "INSERT INTO archive_entries (file_id, session_id, tar_path, status)
         VALUES (?1, ?2, ?3, 'pending')",
        params![file_id.0, session_id, tar_path],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn mark_entry_done(conn: &Connection, entry_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE archive_entries SET status = 'done' WHERE id = ?1",
        params![entry_id],
    )?;
    Ok(())
}

pub fn open_session(conn: &Connection) -> Result<Option<ArchiveSession>> {
    conn.query_row(
        "SELECT id, archive_offset FROM archive_sessions WHERE finalized = 0 ORDER BY id DESC LIMIT 1",
        [],
        |row| {
            Ok(ArchiveSession {
                id: row.get(0)?,
                archive_offset: row.get::<_, i64>(1)? as u64,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

pub fn reset_archive_state(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE files SET phase = 'staged' WHERE phase = 'archived'",
        [],
    )?;
    conn.execute("DELETE FROM archive_entries", [])?;
    conn.execute("DELETE FROM archive_sessions", [])?;
    Ok(())
}

pub fn sum_canonical_bytes_to_archive(conn: &Connection) -> Result<u64> {
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(size), 0)
         FROM files
         WHERE canonical_id = id AND phase IN ('staged', 'archived')",
        [],
        |row| row.get(0),
    )?;
    Ok(total as u64)
}

pub fn sum_archived_canonical_bytes(conn: &Connection) -> Result<u64> {
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(size), 0)
         FROM files
         WHERE canonical_id = id AND phase = 'archived'",
        [],
        |row| row.get(0),
    )?;
    Ok(total as u64)
}
