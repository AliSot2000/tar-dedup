use chrono::Utc;
use rusqlite::{named_params, Connection, OptionalExtension};

use crate::db::types::ArchiveSession;
use crate::error::Result;

pub fn begin_session(conn: &Connection, stream_index: i64, archive_offset: u64) -> Result<i64> {
    conn.execute(
        "INSERT INTO archive_sessions (stream_index, archive_offset, started_at)
         VALUES (:stream_index, :archive_offset, :started_at)",
        named_params! {
            ":stream_index": stream_index,
            ":archive_offset": archive_offset as i64,
            ":started_at": Utc::now().to_rfc3339(),
        },
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn finalize_session(conn: &Connection, session_id: i64, bytes_in: u64, bytes_out: u64) -> Result<()> {
    conn.execute(
        "UPDATE archive_sessions
         SET finalized = 1, bytes_in = :bytes_in, bytes_out = :bytes_out, finished_at = :finished_at
         WHERE id = :id",
        named_params! {
            ":bytes_in": bytes_in as i64,
            ":bytes_out": bytes_out as i64,
            ":finished_at": Utc::now().to_rfc3339(),
            ":id": session_id,
        },
    )?;
    Ok(())
}

pub fn next_stream_index(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(stream_index), -1) + 1 AS next_index FROM archive_sessions",
        [],
        |row| row.get("next_index"),
    )
    .map_err(Into::into)
}

pub fn open_session(conn: &Connection) -> Result<Option<ArchiveSession>> {
    conn.query_row(
        "SELECT id, archive_offset FROM archive_sessions WHERE finalized = 0 ORDER BY id DESC LIMIT 1",
        [],
        |row| {
            Ok(ArchiveSession {
                id: row.get("id")?,
                archive_offset: row.get::<_, i64>("archive_offset")? as u64,
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
    conn.execute("DELETE FROM archive_sessions", [])?;
    Ok(())
}

pub fn sum_canonical_bytes_to_archive(conn: &Connection) -> Result<u64> {
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(size), 0) AS total
         FROM files
         WHERE canonical_id = id AND phase IN ('staged', 'archived')",
        [],
        |row| row.get("total"),
    )?;
    Ok(total as u64)
}

pub fn sum_archived_canonical_bytes(conn: &Connection) -> Result<u64> {
    let total: i64 = conn.query_row(
        "SELECT COALESCE(SUM(size), 0) AS total
         FROM files
         WHERE canonical_id = id AND phase = 'archived'",
        [],
        |row| row.get("total"),
    )?;
    Ok(total as u64)
}
