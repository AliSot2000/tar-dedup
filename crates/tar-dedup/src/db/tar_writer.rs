use std::fs::OpenOptions;
use std::path::Path;

use chrono::Utc;
use rusqlite::{named_params, params, Connection, OptionalExtension};

use crate::db::content_id::original_extension;
use crate::db::flags::FileFlag;
use crate::db::types::{ArchiveSession, FileId};
use crate::error::{Error, Result};

/// `archive_sessions.finalized` values.
pub mod session_status {
    /// Open or interrupted (force abort / crash); cleanup happens at next startup.
    pub const OPEN: i64 = 0;
    /// Compression stream closed successfully (final success or graceful interrupt).
    pub const FINALIZED: i64 = 1;
    /// Startup recovery truncated the incomplete stream; kept for audit.
    pub const ABORTED: i64 = 2;
}

pub fn begin_session(conn: &Connection, stream_index: i64, archive_offset: u64) -> Result<i64> {
    conn.execute(
        "INSERT INTO archive_sessions (stream_index, archive_offset, started_at, finalized)
         VALUES (:stream_index, :archive_offset, :started_at, :finalized)",
        named_params! {
            ":stream_index": stream_index,
            ":archive_offset": archive_offset as i64,
            ":started_at": Utc::now().to_rfc3339(),
            ":finalized": session_status::OPEN,
        },
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn finalize_session(conn: &Connection, session_id: i64, bytes_in: u64, bytes_out: u64) -> Result<()> {
    conn.execute(
        "UPDATE archive_sessions
         SET finalized = :finalized, bytes_in = :bytes_in, bytes_out = :bytes_out, finished_at = :finished_at
         WHERE id = :id AND finalized = :open",
        named_params! {
            ":finalized": session_status::FINALIZED,
            ":bytes_in": bytes_in as i64,
            ":bytes_out": bytes_out as i64,
            ":finished_at": Utc::now().to_rfc3339(),
            ":id": session_id,
            ":open": session_status::OPEN,
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

/// Startup recovery: truncate incomplete stream, mark session aborted, clear pending flags.
pub fn abort_incomplete_session(conn: &Connection, path: &Path, session: &ArchiveSession) -> Result<()> {
    truncate_archive_at(path, session.archive_offset)?;
    mark_session_aborted(conn, session.id)?;
    clear_archive_session_pending(conn)?;
    Ok(())
}

/// Nuclear reset: every archived canonical → staged, wipe all sessions.
pub fn reset_archive_state(conn: &Connection) -> Result<()> {
    let pending = FileFlag::ArchiveSessionPending.mask_i64();
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

/// Staged self-canonical file ids, ordered for packing: extension, size, id.
///
/// Extension matches [`original_extension`] (Rust/`Path`), which SQLite cannot
/// mirror reliably — so we stage `(file_id, ext, size)` in a temp table, then
/// `ORDER BY` and return ids only. Callers re-fetch rows in the write loop so
/// they see up-to-date DB state (and do not keep a full snapshot in RAM).
/// `filter_sha` determines if we filter sha1 IS NOT NULL too. 
pub fn list_staged_canonical_ordered(conn: &Connection, filter_sha: bool) -> Result<Vec<FileId>> {
    let tx = conn.unchecked_transaction()?;

    tx.execute_batch(
        "DROP TABLE IF EXISTS temp.archive_order;
         CREATE TEMP TABLE archive_order (
             file_id INTEGER PRIMARY KEY NOT NULL,
             ext TEXT NOT NULL,
             size INTEGER NOT NULL
         );",
    )?;

    {
        let check = if filter_sha { " AND sha1 IS NOT NULL" } else { "" };
        let mut select = tx.prepare(
            format!("SELECT id, rel_path, size FROM files \
            WHERE canonical_id = id AND phase = 'staged' {check}").as_str(),
        )?;
        let mut insert = tx.prepare(
            "INSERT INTO archive_order (file_id, ext, size) VALUES (?1, ?2, ?3)",
        )?;
        let rows = select.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (id, rel_path, size) = row?;
            let ext = original_extension(Path::new(&rel_path));
            insert.execute(params![id, ext, size])?;
        }
    }

    let out = {
        let mut stmt = tx.prepare(
            "SELECT file_id FROM archive_order
             ORDER BY ext ASC, size ASC, file_id ASC",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, i64>(0).map(FileId))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    tx.execute_batch("DROP TABLE IF EXISTS temp.archive_order;")?;
    tx.commit()?;
    Ok(out)
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

pub fn promote_archive_candidates_to_archived(conn: &Connection, retry: bool) -> Result<u64> {
    let filter_sha = if retry { "OR sha1 IS NULL" } else { "" };
    let stmt = format!(
        "UPDATE files SET phase = 'archived'
         WHERE phase = 'staged'
           AND (
                canonical_id IS NULL OR canonical_id != id
             OR ftype IS NULL OR ftype != 'file'
             {filter_sha}
           )"
    );
    let n = conn.execute(&stmt, {})?;
    Ok(n as u64)
}

pub fn mark_files_archived(conn: &Connection, file_ids: &[crate::db::types::FileId]) -> Result<()> {
    let pending = FileFlag::ArchiveSessionPending.mask_i64();
    for id in file_ids {
        conn.execute(
            "UPDATE files
             SET phase = 'archived',
                 flags = flags & ~:pending
             WHERE id = :id",
            named_params! {
                ":pending": pending,
                ":id": id.0,
            },
        )?;
    }
    Ok(())
}

/// Mark members written into the open session (durable across crash until finalize/abort).
pub fn mark_archive_session_pending(conn: &Connection, file_id: crate::db::types::FileId) -> Result<()> {
    let bit = FileFlag::ArchiveSessionPending.mask_i64();
    conn.execute(
        "UPDATE files SET flags = flags | :bit WHERE id = :id",
        named_params! {
            ":bit": bit,
            ":id": file_id.0,
        },
    )?;
    Ok(())
}

/// After truncate/abort: clear pending; files stay `staged` for rewrite.
pub fn clear_archive_session_pending(conn: &Connection) -> Result<u64> {
    let bit = FileFlag::ArchiveSessionPending.mask_i64();
    let n = conn.execute(
        "UPDATE files SET flags = flags & ~:bit WHERE (flags & :bit) != 0",
        named_params! { ":bit": bit },
    )?;
    Ok(n as u64)
}

/// Truncate archive to `offset` (end of previous finished stream / start of incomplete one).
pub fn truncate_archive_at(path: &Path, offset: u64) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| Error::io(path, e))?;
    file.set_len(offset).map_err(|e| Error::io(path, e))?;
    file.sync_all().map_err(|e| Error::io(path, e))?;
    if offset == 0 {
        // Empty archive file: remove so next session starts clean.
        drop(file);
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}
