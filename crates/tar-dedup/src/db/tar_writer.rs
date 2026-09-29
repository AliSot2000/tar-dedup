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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::db::types::StrippedRecord;

    /// A default staged canonical: self-canonical `staged` file with a digest
    /// and the archive filter passed.
    fn insert_archive_candidate(conn: &Connection, id: i64, qid: i64, name: &str, size: u64) {
        conn.execute(
            "INSERT INTO files (id, abs_path, ext, size, ftype, phase, sha1, \
             include_reason_archive, exclude_reason_archive, flags, canonical_id) \
             VALUES (:id, :abs_path, :ext, :size, 'file', 'staged', :sha1, -1, 0, 0, :id)",
            named_params! {
                ":id": id,
                ":abs_path": format!("/tmp/{name}-{qid}.dat"),
                ":ext": format!(".{name}"),
                ":size": size as i64,
                ":sha1": [7u8; 20].as_slice(),
            },
        ).expect("insert candidate");
    }

    fn set_phase(conn: &Connection, id: i64, phase: &str) {
        conn.execute(
            "UPDATE files SET phase = :phase WHERE id = :id",
            named_params! { ":id": id, ":phase": phase },
        ).expect("set phase");
    }

    fn set_flag(conn: &Connection, id: i64, flag: FileFlag) {
        crate::db::flags::set_file_flag(conn, FileId(id), flag, true).expect("set flag");
    }

    fn row_phase(conn: &Connection, id: i64) -> String {
        conn.query_row(
            "SELECT phase FROM files WHERE id = :id",
            named_params! { ":id": id },
            |row| row.get::<_, String>(0),
        ).expect("read phase")
    }

    fn row_flag(conn: &Connection, id: i64, flag: FileFlag) -> bool {
        conn.query_row(
            "SELECT flags FROM files WHERE id = :id",
            named_params! { ":id": id },
            |row| row.get::<_, i64>(0),
        ).expect("read flags")
            & flag.mask_i64() != 0
    }

    fn open_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = Connection::open(&dir.path().join("t.sqlite")).expect("open conn");
        schema::initialize(&conn).expect("schema init");
        // The internal include rule (`apply_no_filter` hands out
        // `include_reason_archive = -1`), which must satisfy the FK.
        conn.execute(
            "INSERT OR IGNORE INTO filter_reason_archive (id, source, line, expression) \
             VALUES (-1, 'internal', NULL, '.*')",
            [],
        ).expect("seed internal include rule");
        (dir, conn)
    }

    /// Seed the `files` matrix used by the abort/clear tests: (a) pending
    /// non-archived, (b) pending + archived, (c) plain staged, (d) plain archived.
    fn seed_pending_matrix(conn: &Connection) {
        insert_archive_candidate(conn, 1, 1, "a", 10);
        insert_archive_candidate(conn, 2, 2, "b", 20);
        insert_archive_candidate(conn, 3, 3, "c", 30);
        insert_archive_candidate(conn, 4, 4, "d", 40);
        set_flag(conn, 1, FileFlag::AppendedPath);   // (a) pending non-archived
        set_flag(conn, 2, FileFlag::AppendedPath);
        set_phase(conn, 2, "archived");              // (b) pending + archived
        set_phase(conn, 4, "archived");              // (d) plain archived
    }

    #[test]
    fn abort_incomplete_session_resets_only_pending_non_archived() {
        let (_dir, conn) = open_db();
        seed_pending_matrix(&conn);
        conn.execute(
            "INSERT INTO archive_sessions (id, archive_offset, started_at, finalized) \
             VALUES (1, 100, '2026-01-01T00:00:00Z', 0)",
            [],
        ).expect("insert open session");

        abort_incomplete_session(&conn, &ArchiveSession { id: 1, archive_offset: 100 })
            .expect("abort session");

        // (a) reset to staged + flag cleared; (c) untouched; (d) untouched.
        assert_eq!(row_phase(&conn, 1), "staged");
        assert!(!row_flag(&conn, 1, FileFlag::AppendedPath));
        // (b) already archived: flag kept, phase kept.
        assert_eq!(row_phase(&conn, 2), "archived");
        assert!(row_flag(&conn, 2, FileFlag::AppendedPath));
        // (c)/(d) untouched.
        assert_eq!(row_phase(&conn, 3), "staged");
        assert_eq!(row_phase(&conn, 4), "archived");
        // Session marked aborted.
        let finalized: i64 = conn.query_row(
            "SELECT finalized FROM archive_sessions WHERE id = 1",
            [],
            |row| row.get(0),
        ).expect("read session finalized");
        assert_eq!(finalized, session_status::ABORTED);
    }

    #[test]
    fn clear_archive_session_pending_only_resets_non_archived() {
        let (_dir, conn) = open_db();
        seed_pending_matrix(&conn);

        let n = clear_archive_session_pending(&conn).expect("clear pending");

        assert_eq!(n, 1);
        assert_eq!(row_phase(&conn, 1), "staged");
        assert!(!row_flag(&conn, 1, FileFlag::AppendedPath));
        assert!(row_flag(&conn, 2, FileFlag::AppendedPath));   // archived rows keep flag
        assert_eq!(row_phase(&conn, 2), "archived");
    }

    #[test]
    fn reset_archive_state_returns_to_staged_and_wipes_sessions() {
        let (_dir, conn) = open_db();
        seed_pending_matrix(&conn);
        conn.execute(
            "INSERT INTO archive_sessions (id, archive_offset, started_at, finalized) \
             VALUES (1, 100, '2026-01-01T00:00:00Z', 0)",
            [],
        ).expect("insert open session");

        reset_archive_state(&conn).expect("reset state");

        for id in 1..=4 {
            assert_eq!(row_phase(&conn, id), "staged");
            assert!(!row_flag(&conn, id, FileFlag::AppendedPath));
        }
        let n_sessions: i64 = conn.query_row(
            "SELECT COUNT(*) FROM archive_sessions",
            [], |row| row.get(0),
        ).expect("count sessions");
        assert_eq!(n_sessions, 0);
    }

    #[test]
    fn queue_slices_and_skips_outcome_rows() {
        let (_dir, conn) = open_db();
        // Sizes set so the size ordering places 3 before 1 regardless of qid.
        insert_archive_candidate(&conn, 1, 1, "x", 300);
        insert_archive_candidate(&conn, 2, 2, "x", 100);
        insert_archive_candidate(&conn, 3, 3, "x", 200);
        create_archive_queue(&conn).expect("create queue");
        populate_archive_queue(&conn, false).expect("populate queue");

        // 2 already appended last session; 3 errored.
        set_phase(&conn, 2, "archived");
        set_flag(&conn, 3, FileFlag::ErrorWhileArchive);

        let mut all: Vec<(u64, StrippedRecord)> = Vec::new();
        let mut pos = 0u64;
        loop {
            let batch = pull_pending_archive_rows::<StrippedRecord>(&conn, pos, 2).expect("pull");
            if batch.is_empty() { break }
            pos = batch.last().expect("nonempty").0;
            all.extend(batch);
        }

        assert_eq!(all.len(), 1);
        assert_eq!(all[0].1.id, FileId(1));
        // Filtered rows: queued non-staged (2) and errored (3) never surface.
    }

    #[test]
    fn queue_orders_by_ext_size_then_id() {
        let (_dir, conn) = open_db();
        insert_archive_candidate(&conn, 1, 3, "z", 100);   // ext .z
        insert_archive_candidate(&conn, 2, 1, "a", 300);   // ext .a, size 300
        insert_archive_candidate(&conn, 3, 2, "a", 100);   // ext .a, size 100
        create_archive_queue(&conn).expect("create queue");
        populate_archive_queue(&conn, false).expect("populate queue");

        let rows = pull_pending_archive_rows::<StrippedRecord>(&conn, 0, 100).expect("pull");
        let ids: Vec<FileId> = rows.iter().map(|(_p, r)| r.id).collect();
        assert_eq!(ids, vec![FileId(3), FileId(2), FileId(1)]);
    }

    #[test]
    fn queue_orders_by_basename_when_sorted_by_name() {
        let (_dir, conn) = open_db();
        insert_archive_candidate(&conn, 1, 1, "z", 100);   // basename z-1.dat
        insert_archive_candidate(&conn, 2, 2, "a", 300);   // basename a-2.dat
        insert_archive_candidate(&conn, 3, 3, "m", 100);   // basename m-3.dat
        create_archive_queue(&conn).expect("create queue");
        populate_archive_queue(&conn, true).expect("populate queue");

        let rows = pull_pending_archive_rows::<StrippedRecord>(&conn, 0, 100).expect("pull");
        let ids: Vec<FileId> = rows.iter().map(|(_p, r)| r.id).collect();
        assert_eq!(ids, vec![FileId(2), FileId(3), FileId(1)]);
    }

    #[test]
    fn populate_archive_queue_is_idempotent() {
        let (_dir, conn) = open_db();
        insert_archive_candidate(&conn, 1, 1, "x", 100);
        insert_archive_candidate(&conn, 2, 2, "x", 200);
        create_archive_queue(&conn).expect("create queue");
        populate_archive_queue(&conn, false).expect("first populate");
        let second_run = populate_archive_queue(&conn, false).expect("second populate");

        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM archive_queue",
            [], |row| row.get(0),
        ).expect("count queue");
        assert_eq!(n, 2);
        assert_eq!(second_run, 0);   // nothing new inserted
    }

    #[test]
    fn drop_archive_queue_is_idempotent() {
        let (_dir, conn) = open_db();
        create_archive_queue(&conn).expect("create queue");
        drop_archive_queue(&conn).expect("first drop");
        drop_archive_queue(&conn).expect("second drop");
        // Dropping an absent table does not error; re-creating works after.
        create_archive_queue(&conn).expect("recreate queue");
    }
}