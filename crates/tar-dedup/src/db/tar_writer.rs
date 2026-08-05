use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, named_params};

use crate::db::flags::FileFlag;
use crate::db::meta;
use crate::db::types::{ArchiveSession, FileId};
use crate::error::Result;

/// `archive_sessions.finalized` values.
pub mod session_status {
    /// Open or interrupted (force abort / crash); cleanup happens at next startup.
    pub const OPEN: i64 = 0;
    /// Compression stream closed successfully (final success or graceful interrupt).
    pub const FINALIZED: i64 = 1;
    /// Startup recovery truncated the incomplete stream; kept for audit.
    pub const ABORTED: i64 = 2;
}

pub fn begin_session(conn: &Connection, archive_offset: u64) -> Result<i64> {
    conn.execute(
        "INSERT INTO archive_sessions (archive_offset, started_at, finalized)
         VALUES (:archive_offset, :started_at, :finalized)",
        named_params! {
            ":archive_offset": archive_offset as i64,
            ":started_at": Utc::now().to_rfc3339(),
            ":finalized": session_status::OPEN,
        },
    )?;
    Ok(conn.last_insert_rowid())
}

/// Tentative `finished_at` while session remains OPEN (for in-tar snapshot).
pub fn stamp_session_finished_at(conn: &Connection, session_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE archive_sessions
         SET finished_at = :finished_at
         WHERE id = :id AND finalized = :open",
        named_params! {
            ":finished_at": Utc::now().to_rfc3339(),
            ":id": session_id,
            ":open": session_status::OPEN,
        },
    )?;
    Ok(())
}

/// Mark session finalized after the compression stream has closed.
pub fn finalize_session(conn: &Connection, session_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE archive_sessions
         SET finalized = :finalized, finished_at = :finished_at
         WHERE id = :id AND finalized = :open",
        named_params! {
            ":finalized": session_status::FINALIZED,
            ":finished_at": Utc::now().to_rfc3339(),
            ":id": session_id,
            ":open": session_status::OPEN,
        },
    )?;
    Ok(())
}

pub fn open_session(conn: &Connection) -> Result<Option<ArchiveSession>> {
    conn.query_row(
        "SELECT id, archive_offset FROM archive_sessions
         WHERE finalized = :open
         ORDER BY id DESC LIMIT 1",
        named_params! { ":open": session_status::OPEN },
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

pub fn has_finalized_session(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM archive_sessions WHERE finalized = :finalized",
        named_params! { ":finalized": session_status::FINALIZED },
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

/// Mark an open session as recovered/aborted (audit row; not deleted).
pub fn mark_session_aborted(conn: &Connection, session_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE archive_sessions
         SET finalized = :aborted, finished_at = :finished_at
         WHERE id = :id AND finalized = :open",
        named_params! {
            ":aborted": session_status::ABORTED,
            ":finished_at": Utc::now().to_rfc3339(),
            ":id": session_id,
            ":open": session_status::OPEN,
        },
    )?;
    Ok(())
}

/// Startup recovery: mark session aborted, clear pending flags.
pub fn abort_incomplete_session(conn: &Connection, session: &ArchiveSession) -> Result<()> {
    mark_session_aborted(conn, session.id)?;
    clear_archive_session_pending(conn)?;
    Ok(())
}

/// Nuclear reset: every archived canonical → staged, wipe all sessions.
pub fn reset_archive_state(conn: &Connection) -> Result<()> {
    let pending = FileFlag::AppendedPath.mask_i64();
    conn.execute(
        "UPDATE files
         SET phase = 'staged',
             flags = flags & ~:pending
         WHERE phase = 'archived' OR (flags & :pending) != 0",
        named_params! { ":pending": pending },
    )?;
    conn.execute("DELETE FROM archive_sessions", [])?;
    Ok(())
}

pub fn sum_canonical_bytes_to_archive(conn: &Connection, filter_sha: bool) -> Result<u64> {
    let filter = if filter_sha {
        " AND sha1 IS NOT NULL"
    } else {
        ""
    };
    let total: i64 = conn.query_row(&format!(
        "SELECT COALESCE(SUM(size), 0) AS total
         FROM files
         WHERE canonical_id = id 
            AND phase IN ('staged', 'archived') 
            {filter} 
            AND NOT (ftype IS NULL OR ftype != 'file')"),
        [],
        |row| row.get("total"),
    )?;
    Ok(total as u64)
}

/// Staged self-canonical file ids, ordered for packing: extension, size, id.
/// When `filter_sha` is true, rows with `sha1 IS NULL` are omitted.
pub fn list_staged_canonical_ordered(conn: &Connection, filter_sha: bool) -> Result<Vec<FileId>> {
    let filter = if filter_sha {
        " AND sha1 IS NOT NULL"
    } else {
        ""
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT id FROM files
         WHERE canonical_id = id 
            AND phase = 'staged' 
            {filter} 
            AND NOT (ftype IS NULL OR ftype != 'file')
         ORDER BY ext ASC, size ASC, id ASC"
    ))?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0).map(FileId))?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn sum_archived_canonical_bytes(conn: &Connection, filter_sha: bool) -> Result<u64> {
    let filter = if filter_sha {
        " AND sha1 IS NOT NULL"
    } else {
        ""
    };
    let total: i64 = conn.query_row(&format!(
        "SELECT COALESCE(SUM(size), 0) AS total
         FROM files
         WHERE canonical_id = id 
            AND phase = 'archived' 
            {filter}
            AND NOT (ftype IS NULL OR ftype != 'file')"),
        [],
        |row| row.get("total"),
    )?;
    Ok(total as u64)
}

/// Move staged rows that will never be tar payloads to `archived`.
///
/// Ineligible: non-self-canonical, non-file types, and (when `filter_sha`) missing `sha1`.
/// Does not touch outcome flags — phase is flow only.
pub fn promote_ineligible_to_archived(conn: &Connection, filter_sha: bool) -> Result<u64> {
    let sha_clause = if filter_sha {
        " OR sha1 IS NULL"
    } else {
        ""
    };
    let stmt = format!(
        "UPDATE files SET phase = 'archived'
         WHERE phase = 'staged'
           AND (
                canonical_id IS NULL OR canonical_id != id
             OR ftype IS NULL OR ftype != 'file'
             {sha_clause}
           )"
    );
    let n = conn.execute(&stmt, {})?;
    Ok(n as u64)
}

/// After every eligible file has been considered (written or flagged error), advance
/// remaining flow to `archived`. Does not clear or set outcome flags.
pub fn promote_remainder_to_archived(conn: &Connection) -> Result<u64> {
    let n = conn.execute(
        "UPDATE files SET phase = 'archived' WHERE phase != 'archived'",
        [],
    )?;
    Ok(n as u64)
}

/// Promote all `AppendedPath` rows to `archived`.
/// Leaves `AppendedPath` set (sticky proof the payload was written); abort recovery
/// only clears the flag for rows that are still not `archived`.
pub fn promote_pending_archived(conn: &Connection) -> Result<u64> {
    let pending = FileFlag::AppendedPath.mask_i64();
    let n = conn.execute(
        "UPDATE files
         SET phase = 'archived'
         WHERE (flags & :pending) != 0
           AND phase != 'archived'",
        named_params! { ":pending": pending },
    )?;
    Ok(n as u64)
}

/// Mark members written into the open session (durable across crash until finalize/abort).
pub fn mark_archive_session_pending(conn: &Connection, file_id: FileId) -> Result<()> {
    let bit = FileFlag::AppendedPath.mask_i64();
    conn.execute(
        "UPDATE files SET flags = flags | :bit WHERE id = :id",
        named_params! {
            ":bit": bit,
            ":id": file_id.0,
        },
    )?;
    Ok(())
}

/// After truncate/abort: clear pending on non-archived rows; files stay `staged` for rewrite.
/// Does not touch `AppendedPath` on rows already promoted to `archived`.
pub fn clear_archive_session_pending(conn: &Connection) -> Result<u64> {
    let bit = FileFlag::AppendedPath.mask_i64();
    let n = conn.execute(
        "UPDATE files
         SET flags = flags & ~:bit
         WHERE (flags & :bit) != 0
           AND phase != 'archived'",
        named_params! { ":bit": bit },
    )?;
    Ok(n as u64)
}

pub fn get_archive_bytes_in(conn: &Connection) -> Result<u64> {
    Ok(meta::get_tar_writer_bytes_in(conn)?.unwrap_or(0))
}

pub fn get_archive_bytes_out(conn: &Connection) -> Result<Option<u64>> {
    meta::get_tar_writer_bytes_out(conn)
}

pub fn set_archive_bytes_in(conn: &Connection, value: u64) -> Result<()> {
    meta::set_tar_writer_bytes_in(conn, value)
}

pub fn set_archive_bytes_out(conn: &Connection, value: u64) -> Result<()> {
    meta::set_tar_writer_bytes_out(conn, value)
}
