use rusqlite::{Connection, named_params};

use crate::db::common::SqlFileRow;
use crate::db::common::generate_archive_filter;
use crate::db::types::FileId;
use crate::error::{Result, ToPanic};

pub fn promote_unstageable_files(conn: &Connection) -> Result<u64> {
    let filter_rows = generate_archive_filter(None);
    let n = conn.execute(
        &format!(
            "UPDATE files SET phase = 'staged'
        WHERE phase = 'sparsified'
        AND (
            ftype != 'file'
            OR canonical_id IS NULL
            OR canonical_id != id
            OR sha1 IS NULL
            OR NOT ({filter_rows})
        )"
        ),
        [],
    ).to_panic()?;
    Ok(n as u64)
}

/// Number of rows that will ever be staged (across sessions): eligible
/// canonical files currently either `sparsified` (todo) or already `staged`
/// (done in a prior run). Ineligible rows promoted to `staged` never match.
pub fn count_all_stage_candidates(conn: &Connection) -> Result<u64> {
    let filter_rows = generate_archive_filter(None);
    let count: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) AS count FROM files \
            WHERE (phase = 'sparsified' OR phase = 'staged') \
                AND ftype = 'file' \
                AND canonical_id = id \
                AND ({filter_rows}) \
                AND sha1 IS NOT NULL"
        ),
        [],
        |row| row.get("count"),
    ).to_panic()?;
    Ok(count as u64)
}

/// Cursor-backed pull of rows waiting to be staged, slicing by id.
/// `last_id` = 0 starts at the beginning; each batch returns up to `limit`
/// rows with id strictly greater than `last_id`, ordered by id. IDs never
/// change mid-phase so the cursor is stable across batches.
pub fn list_files_to_stage_after<R: SqlFileRow>(
    conn: &Connection, last_id: &FileId, limit: u64) -> Result<Vec<R>> {
    debug_assert!(last_id.0 >= 0,
                  "INVARIANT ERROR: Only > 0 FileIds handed out, 0 minimum lower bound");
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM files \
            WHERE phase = 'sparsified' \
                AND ftype = 'file' \
                AND canonical_id = id \
                AND {} \
                AND sha1 IS NOT NULL \
                AND id > :last \
            ORDER BY id LIMIT :limit",
        R::sql_columns(None),
        generate_archive_filter(None)
    )).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":last": last_id.0,
            ":limit": limit
        },
        |row| R::from_row(row, None),
    ).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

// TODO fix this up, this is not correct.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::db::types::StrippedRecord;

    fn open_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = Connection::open(&dir.path().join("t.sqlite")).expect("open conn");
        schema::initialize(&conn).expect("schema init");
        // The internal include rule `apply_no_filter` hands out
        // (`include_reason_archive = -1`), which must satisfy the FK.
        conn.execute(
            "INSERT OR IGNORE INTO filter_reason_archive (id, source, line, expression) \
             VALUES (-1, 'internal', NULL, '.*')",
            [],
        ).expect("seed internal include rule");
        (dir, conn)
    }

    /// A default stage candidate: self-canonical `sparsified` file with a digest.
    fn insert_candidate(conn: &Connection, id: i64) {
        conn.execute(
            "INSERT INTO files (id, abs_path, ext, size, ftype, phase, sha1, \
             include_reason_archive, exclude_reason_archive, flags, canonical_id) \
             VALUES (:id, :abs_path, '.bin', 1024, 'file', 'sparsified', :sha1, \
                     -1, 0, 0, :id)",
            named_params! {
                ":id": id,
                ":abs_path": format!("/tmp/st-{id}.bin"),
                ":sha1": [7u8; 20].as_slice(),
            },
        ).expect("insert candidate");
    }

    fn stage_ids(conn: &Connection, last_id: i64, limit: u64) -> Vec<i64> {
        list_files_to_stage_after::<StrippedRecord>(conn, &FileId(last_id), limit)
            .expect("list")
            .into_iter()
            .map(|r| r.id.0)
            .collect::<Vec<i64>>()
    }

    #[test]
    fn list_files_to_stage_after_slices_and_orders() {
        let (_dir, conn) = open_db();
        for id in [1, 2, 3, 4] {
            insert_candidate(&conn, id);
        }

        assert_eq!(stage_ids(&conn, 0, 2), [1, 2]);
        assert_eq!(stage_ids(&conn, 2, 2), [3, 4]);
        assert!(stage_ids(&conn, 4, 2).is_empty());
    }

    #[test]
    fn list_files_to_stage_after_excludes_promoted_errored_and_ineligible() {
        let (_dir, conn) = open_db();
        for id in [1, 2, 3, 4, 5, 6] {
            insert_candidate(&conn, id);
        }
        conn.execute("UPDATE files SET phase = 'staged' WHERE id = 2", []).expect("staged");
        conn.execute("UPDATE files SET canonical_id = 1 WHERE id = 3", []).expect("non-canonical");
        conn.execute("UPDATE files SET ftype = 'dir' WHERE id = 4", []).expect("dir");
        conn.execute("UPDATE files SET sha1 = NULL WHERE id = 5", []).expect("null sha");
        conn.execute("UPDATE files SET include_reason_archive = 0 WHERE id = 6", []).expect("filtered");

        assert_eq!(stage_ids(&conn, 0, 100), [1]);
    }

    #[test]
    fn count_all_stage_candidates_spans_sessions() {
        let (_dir, conn) = open_db();
        insert_candidate(&conn, 1);   // staged in a prior session
        insert_candidate(&conn, 2);   // todo this session
        insert_candidate(&conn, 3);   // todo this session
        conn.execute("UPDATE files SET phase = 'staged' WHERE id = 1", []).expect("staged");

        // Ineligible rows promoted to `staged` must not inflate the workload.
        conn.execute(
            "INSERT INTO files (id, abs_path, ext, size, ftype, phase) \
             VALUES (4, '/tmp/dir', '', 0, 'dir', 'staged')",
            [],
        ).expect("ineligible promoted row");

        assert_eq!(count_all_stage_candidates(&conn).expect("count"), 3);
    }

    #[test]
    fn promote_unstageable_files_promotes_ineligible_arms() {
        let (_dir, conn) = open_db();
        // Row 1 is the only valid candidate (stays sparsified).
        insert_candidate(&conn, 1);
        // One row per promote OR arm.
        insert_candidate(&conn, 2);
        conn.execute("UPDATE files SET ftype = 'dir' WHERE id = 2", []).expect("arm dir");
        insert_candidate(&conn, 3);
        conn.execute("UPDATE files SET canonical_id = NULL WHERE id = 3", []).expect("arm canon null");
        insert_candidate(&conn, 4);
        conn.execute("UPDATE files SET canonical_id = 1 WHERE id = 4", []).expect("arm canon != id");
        insert_candidate(&conn, 5);
        conn.execute("UPDATE files SET sha1 = NULL WHERE id = 5", []).expect("arm null sha");
        insert_candidate(&conn, 6);
        conn.execute(
            "UPDATE files SET include_reason_archive = 0 WHERE id = 6", [],
        ).expect("arm filter fail");

        let promoted = promote_unstageable_files(&conn).expect("promote");

        assert_eq!(promoted, 5);
        for id in 2..7 {
            assert_eq!(row_phase(&conn, id), "staged");
        }
        assert_eq!(row_phase(&conn, 1), "sparsified");
        assert_eq!(count_all_stage_candidates(&conn).expect("count"), 1);
    }

    fn row_phase(conn: &Connection, id: i64) -> String {
        conn.query_row(
            "SELECT phase FROM files WHERE id = :id",
            named_params! { ":id": id },
            |row| row.get::<_, String>(0),
        ).expect("read phase")
    }
}
