use chrono::Utc;
use rusqlite::{Connection, OptionalExtension, named_params};

use crate::db::common::{SqlFileRow, generate_archive_filter};
use crate::db::flags::FileFlag;
use crate::db::meta;
use crate::db::types::{ArchiveSession, FileId};
use crate::error::{Result, ToPanic};

// -------------------------------------------------------------------------------------------------
// Session Logic
// -------------------------------------------------------------------------------------------------

/// `archive_sessions.finalized` values.
pub mod session_status {
    /// Open or interrupted (force abort / crash); cleanup happens at next startup.
    pub const OPEN: i64 = 0;
    /// Compression stream closed successfully (final success or graceful interrupt).
    pub const FINALIZED: i64 = 1;
    /// Startup recovery truncated the incomplete stream; kept for audit.
    pub const ABORTED: i64 = 2;
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


pub fn begin_session(conn: &Connection, archive_offset: u64) -> Result<i64> {
    conn.execute(
        "INSERT INTO archive_sessions (archive_offset, started_at, finalized)
         VALUES (:archive_offset, :started_at, :finalized)",
        named_params! {
            ":archive_offset": archive_offset as i64,
            ":started_at": Utc::now().to_rfc3339(),
            ":finalized": session_status::OPEN,
        },
    ).to_panic()?;
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
    ).to_panic()?;
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
    ).to_panic()?;
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
    .to_panic()
    .map_err(Into::into)
}

pub fn has_finalized_session(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM archive_sessions WHERE finalized = :finalized",
        named_params! { ":finalized": session_status::FINALIZED },
        |row| row.get(0),
    ).to_panic()?;
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
    ).to_panic()?;
    Ok(())
}

/// Startup recovery: mark session aborted, clear pending flags.
pub fn abort_incomplete_session(conn: &Connection, session: &ArchiveSession) -> Result<()> {
    mark_session_aborted(conn, session.id)?;
    clear_archive_session_pending(conn)?;
    Ok(())
}

// -------------------------------------------------------------------------------------------------
// File queries
// -------------------------------------------------------------------------------------------------

/// Nuclear reset: every archived canonical → staged, wipe all sessions.
pub fn reset_archive_state(conn: &Connection) -> Result<()> {
    let pending = FileFlag::AppendedPath.mask_i64();
    conn.execute(
        "UPDATE files
         SET phase = 'staged',
             flags = flags & ~:pending
         WHERE phase = 'archived' OR (flags & :pending) != 0",
        named_params! { ":pending": pending },
    ).to_panic()?;
    conn.execute("DELETE FROM archive_sessions", []).to_panic()?;
    Ok(())
}

pub fn sum_canonical_bytes_to_archive(conn: &Connection) -> Result<u64> {
    let total: i64 = conn.query_row(
        &format!(
            "SELECT COALESCE(SUM(size), 0) AS total
            FROM files
            WHERE canonical_id = id
                AND phase IN ('staged', 'archived')
                AND sha1 IS NOT NULL
                AND ftype = 'file'
                AND {}",
            generate_archive_filter(None)
        ),
        [],
        |row| row.get("total"),
    ).to_panic()?;
    Ok(total as u64)
}

pub fn sum_archived_canonical_bytes(conn: &Connection) -> Result<u64> {
    let total: i64 = conn.query_row(
        &format!(
            "SELECT COALESCE(SUM(size), 0) AS total
            FROM files
            WHERE canonical_id = id
                AND phase = 'archived'
                AND sha1 IS NOT NULL
                AND ftype = 'file'
                AND {}",
            generate_archive_filter(None)
        ),
        [],
        |row| row.get("total"),
    ).to_panic()?;
    Ok(total as u64)
}

pub fn promote_to_archived(conn: &Connection, id: &FileId) -> Result<u64> {
    let tot = conn.execute(
            "UPDATE files SET phase = 'archived' WHERE id = :id",
            named_params! { ":id" : id.0 },
    ).to_panic()?;
    Ok(tot as u64)
}

/// Shared WHERE for the archive-quened payloads: self-canonical staged regular
/// files that pass the archive filter.
fn archive_queue_where() -> String {
    format!(
        "canonical_id = id
         AND phase = 'staged'
         AND sha1 IS NOT NULL
         AND ftype = 'file'
         AND {}",
        generate_archive_filter(None)
    )
}

/// Create the `archive_queue` ordering table. Idempotent.
pub fn create_archive_queue(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS archive_queue (
             id      INTEGER PRIMARY KEY,
             file_id INTEGER NOT NULL UNIQUE REFERENCES files(id)
         )",
        [],
    ).to_panic()?;
    Ok(())
}

/// Populate `archive_queue` with the full, stable set of staged canonicals in
/// `ext, size, id` order (or by basename when `sort_by_name` is true) — position
/// = `row_number()`. Idempotent: `INSERT OR IGNORE` (via `UNIQUE(file_id)`) keeps
/// rows from an earlier populate (e.g. a resume) and adds only missing ones.
pub fn populate_archive_queue(conn: &Connection, sort_by_name: bool) -> Result<u64> {
    let order = if sort_by_name {
        "ext ASC, replace(abs_path, rtrim(abs_path, replace(abs_path, '/', '')), '') ASC, \
         size ASC, id ASC"
    } else {
        "ext ASC, size ASC, id ASC"
    };
    let sql = format!(
        "INSERT OR IGNORE INTO archive_queue (id, file_id)
         SELECT row_number() OVER (ORDER BY {order}), id
         FROM files WHERE {}",
        archive_queue_where()
    );
    let n = conn.execute(&sql, []).to_panic()?;
    Ok(n as u64)
}

/// Next slice of still-pending archive rows, in `archive_queue` order.
///
/// Joins the queue against `files`, returning only rows still needing a tar
/// payload (`phase = 'staged'`, no AppendedPath, no ErrorWhileArchive), so a
/// resume skips already-appended members; the queue supplies the ordering. The
/// returned `u64` is the queue position of each row, letting the caller advance
/// the read `index` across slices. Same cursor contract as the sparsify queue;
/// see `db/sparsify.rs`.
pub fn pull_pending_archive_rows<R: SqlFileRow>(
    conn: &Connection,
    index: u64,
    limit: u64,
) -> Result<Vec<(u64, R)>> {
    let cols = R::sql_columns(Some("files"));
    let sql = format!(
        "SELECT archive_queue.id AS pos, {cols}
         FROM files JOIN archive_queue ON archive_queue.file_id = files.id
         WHERE archive_queue.id > :index
           AND files.phase = 'staged'
           AND (files.flags & :appended) = 0
           AND (files.flags & :error_flag) = 0
         ORDER BY archive_queue.id
         LIMIT :limit"
    );
    let mut stmt = conn.prepare(&sql).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":index": index as i64,
            ":appended": FileFlag::AppendedPath.mask_i64(),
            ":error_flag": FileFlag::ErrorWhileArchive.mask_i64(),
            ":limit": limit,
        },
        |r: &rusqlite::Row<'_>| {
            let pos = r.get::<_, i64>("pos")? as u64;
            let record = R::from_row(r, Some("files"))?;
            Ok((pos, record))
        },
    ).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// Drop the `archive_queue` ordering table. Idempotent. Call on the phase-success
/// path only; leave it in place across interrupts/resumes so a repopulate is a
/// no-op for already-queued rows.
pub fn drop_archive_queue(conn: &Connection) -> Result<()> {
    conn.execute("DROP TABLE IF EXISTS archive_queue", []).to_panic()?;
    Ok(())
}

/// Move staged rows that will never be tar payloads to `archived`.
///
/// Ineligible: non-self-canonical, non-file types, and (when `filter_sha`) missing `sha1`.
/// Does not touch outcome flags — phase is flow only.
pub fn promote_ineligible_to_archived(conn: &Connection) -> Result<u64> {
    let stmt =
        "UPDATE files SET phase = 'archived'
        WHERE phase = 'staged'
            AND (canonical_id IS NULL
                OR canonical_id != id
                OR ftype != 'file'
                OR sha1 IS NULL)";
    let n = conn.execute(&stmt, {}).to_panic()?;
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
    ).to_panic()?;
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
    ).to_panic()?;
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
    ).to_panic()?;
    Ok(n as u64)
}