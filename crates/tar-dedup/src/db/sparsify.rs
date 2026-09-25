use rusqlite::{Connection, named_params};

use crate::db::common::{SqlFileRow, generate_archive_filter, mark_phase};
use crate::db::flags::{FileFlag, set_file_flag};
use crate::db::types::{FileId, FilePhase};
use crate::error::{Error, Result, ToPanic};

/// Result of a sparsify worker run for one file:
/// - `err: None` — the sparse rewrite succeeded (`HasSparse` + `sparsified`);
/// - `Some(Error::FileStat(_))` — per-file failure (`ErrorWhileSparsify` + `sparsified`);
/// - `Some(Error::Interrupted)` — aborted in-flight (row stays `deduped`).
pub struct SparseOutcome {
    pub id: FileId,
    pub modified: bool,
    pub err: Option<Error>,
}

/// Advance every `deduped` row to `sparsified` (no HasSparse / canonical changes).
pub fn promote_deduped_to_sparsified(conn: &Connection) -> Result<u64> {
    let n = conn.execute(
        "UPDATE files SET phase = 'sparsified' WHERE phase = 'deduped'",
        [],
    ).to_panic()?;
    Ok(n as u64)
}

/// Promote Deduped rows that are **not** sparsify candidates (null-safe negation).
pub fn promote_non_sparsify_candidates_to_sparsified(
    conn: &Connection,
    min_pages: u64,
) -> Result<u64> {
    let has_sparse = FileFlag::HasSparse.mask_i64();
    let filtered_rows = generate_archive_filter(None);
    let n = conn.execute(&format!(
        "UPDATE files SET phase = 'sparsified'
         WHERE phase = 'deduped'
           AND (
             canonical_id IS NULL
             OR canonical_id != id
             OR ftype != 'file'
             OR sha1 IS NULL               -- technically implied by canonical_id IS NULL
             OR sparse_count IS NULL       -- sparse_count IS NULL implied by canonical_id IS NULL
             OR sparse_count < :min_pages
             OR (flags & :has_sparse) != 0
             OR NOT ({filtered_rows})
         )"),
        named_params! {
            ":min_pages": min_pages as i64,
            ":has_sparse": has_sparse,
        },
    ).to_panic()?;
    Ok(n as u64)
}

/// Shared WHERE for the sparsify candidates — self-canonical deduped regular
/// files with enough empty pages, no sparse rewrite yet, archive-filter passed.
/// Params: `:min_pages`, `:has_sparse`.
fn sparsify_candidates_where() -> String {
    format!(
        "phase = 'deduped'
         AND canonical_id = id
         AND ftype = 'file'
         AND sparse_count >= :min_pages -- implies NOT NULL
         AND (flags & :has_sparse) = 0
         AND {}",
        generate_archive_filter(None)
    )
}

/// Create the `sparsify_queue` ordering table. Idempotent.
pub fn create_sparsify_queue(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS sparsify_queue (
             id      INTEGER PRIMARY KEY,
             file_id INTEGER NOT NULL UNIQUE REFERENCES files(id)
         )",
        [],
    ).to_panic()?;
    Ok(())
}

/// Populate `sparsify_queue` with the full, stable set of sparsify candidates in
/// `size DESC, id` order (position = `row_number()`). Idempotent: `INSERT OR
/// IGNORE` (via `UNIQUE(file_id)`) keeps rows from an earlier populate (e.g. a
/// resume) and adds only missing ones.
pub fn populate_sparsify_queue(conn: &Connection, min_pages: u64) -> Result<u64> {
    let sql = format!(
        "INSERT OR IGNORE INTO sparsify_queue (id, file_id)
         SELECT row_number() OVER (ORDER BY size DESC, id), id
         FROM files WHERE {}",
        sparsify_candidates_where()
    );
    let n = conn.execute(
        &sql,
        named_params! {
            ":min_pages": min_pages as i64,
            ":has_sparse": FileFlag::HasSparse.mask_i64(),
        },
    ).to_panic()?;
    Ok(n as u64)
}

/// Next slice of still-pending sparsify rows, in `sparsify_queue` (size-DESC)
/// order.
///
/// Joins the queue against `files`, returning only rows not yet sparsified
/// (`phase = 'deduped'`, no error flag), so a resume skips already-done files;
/// the queue only supplies the ordering. The returned `u64` is the queue
/// position of each row, letting the caller advance the read `index` across
/// slices. Same cursor contract as the hash queue; see `db/hash.rs`.
pub fn pull_pending_sparsify_rows<R: SqlFileRow>(
    conn: &Connection,
    index: u64,
    limit: u64,
) -> Result<Vec<(u64, R)>> {
    let cols = R::sql_columns(Some("files"));
    let sql = format!(
        "SELECT sparsify_queue.id AS pos, {cols}
         FROM files JOIN sparsify_queue ON sparsify_queue.file_id = files.id
         WHERE sparsify_queue.id > :index
           AND files.phase = 'deduped'
           AND (files.flags & :error_flag) = 0
         ORDER BY sparsify_queue.id
         LIMIT :limit"
    );
    let mut stmt = conn.prepare(&sql).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":index": index as i64,
            ":error_flag": FileFlag::ErrorWhileSparsify.mask_i64(),
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

/// Drop the `sparsify_queue` ordering table. Idempotent. Call on the
/// phase-success path only; leave it in place across interrupts/resumes so a
/// repopulate is a no-op for already-queued rows.
pub fn drop_sparsify_queue(conn: &Connection) -> Result<()> {
    conn.execute("DROP TABLE IF EXISTS sparsify_queue", []).to_panic()?;
    Ok(())
}

/// Count rows still awaiting a sparse rewrite — matches `pull_…` without the
/// limit. Errored rows already leave `deduped` at ingest, so the extra
/// `ErrorWhileSparsify` exclusion is belt-and-suspenders parity with `pull`.
pub fn count_pending_sparsify_candidates(conn: &Connection, min_pages: u64) -> Result<u64> {
    let sql = format!(
        "SELECT COUNT(*) AS count FROM files WHERE {} AND (flags & :error_flag) = 0",
        sparsify_candidates_where()
    );
    let count: i64 = conn.query_row(
        &sql,
        named_params! {
            ":min_pages": min_pages as i64,
            ":has_sparse": FileFlag::HasSparse.mask_i64(),
            ":error_flag": FileFlag::ErrorWhileSparsify.mask_i64(),
        },
        |row| row.get("count"),
    ).to_panic()?;
    Ok(count as u64)
}

/// Apply one batch of worker outcomes in a single transaction.
/// `None` err → `HasSparse` + `sparsified`; `FileStat` err → `ErrorWhileSparsify`
/// + `sparsified`; `Interrupted` → untouched (row stays `deduped` for the
/// resume). Returns the number of rows advanced to `sparsified`.
pub fn ingest_sparsify_outcome(
    conn: &mut Connection,
    results: &Vec<SparseOutcome>,
) -> Result<u64> {
    let tx = conn.transaction().to_panic()?;
    let mut resolved = 0u64;
    for outcome in results.iter() {
        if outcome.modified {
            assert_eq!(1, set_file_flag(&tx, outcome.id, FileFlag::Modified, true)?);
        }
        match &outcome.err {
            None => {
                assert_eq!(1, set_file_flag(&tx, outcome.id, FileFlag::HasSparse, true)?);
                mark_phase(&tx, outcome.id, FilePhase::Sparsified)?;
                resolved += 1;
            }
            Some(e) => match e {
                Error::FileStat(_) => {
                    assert_eq!(1, set_file_flag(
                        &tx, outcome.id, FileFlag::ErrorWhileSparsify, true)?);
                    mark_phase(&tx, outcome.id, FilePhase::Sparsified)?;
                    resolved += 1;
                }
                Error::Interrupted => (),
                other => panic!(
                    "Invariant Error. Only FileStatError and Interrupted expected. Got: {other}"
                ),
            },
        }
    }
    tx.commit().to_panic()?;
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::db::types::StrippedRecord;
    use crate::error::{FileStatError, Result};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::PathBuf;

    const MIN_PAGES: u64 = 4;
    const FLAG_HAS_SPARSE: i64 = 1i64 << 9;
    const FLAG_ERR_SPARSIFY: i64 = 1i64 << 10;
    const FLAG_MODIFIED: i64 = 1i64 << 5;

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

    /// A default sparsify candidate: self-canonical `deduped` file with a
    /// digest, `sparse_count` empty pages and the archive filter passed.
    fn insert_candidate(conn: &Connection, id: i64, size: u64, sparse_count: u64) {
        conn.execute(
            "INSERT INTO files (id, abs_path, ext, size, ftype, phase, sha1, \
             sparse_count, include_reason_archive, exclude_reason_archive, flags, canonical_id) \
             VALUES (:id, :abs_path, '.bin', :size, 'file', 'deduped', :sha1, \
                     :sparse_count, -1, 0, 0, :id)",
            named_params! {
                ":id": id,
                ":abs_path": format!("/tmp/sp-{id}.bin"),
                ":size": size as i64,
                ":sparse_count": sparse_count as i64,
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
        set_file_flag(conn, FileId(id), flag, true).expect("set flag");
    }

    fn row_phase(conn: &Connection, id: i64) -> String {
        conn.query_row(
            "SELECT phase FROM files WHERE id = :id",
            named_params! { ":id": id },
            |row| row.get::<_, String>(0),
        ).expect("read phase")
    }

    fn row_flags(conn: &Connection, id: i64) -> i64 {
        conn.query_row(
            "SELECT flags FROM files WHERE id = :id",
            named_params! { ":id": id },
            |row| row.get(0),
        ).expect("read flags")
    }

    fn file_stat_err() -> Error {
        Error::FileStat(FileStatError::Io {
            path: PathBuf::from("/tmp/nope.bin"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied".to_string()),
        })
    }

    #[test]
    fn promote_covers_every_or_disjunct_and_keeps_candidate() {
        let (_dir, conn) = open_db();

        // Row per promote OR arm (all `deduped`; the arm itself is the only
        // failing precondition).
        insert_candidate(&conn, 1, 1024, 8);
        conn.execute("UPDATE files SET canonical_id = NULL WHERE id = 1", []).expect("arm canon null");
        insert_candidate(&conn, 2, 1024, 8);
        conn.execute(
            "UPDATE files SET canonical_id = 1 WHERE id = 2", [],
        ).expect("arm canon != id");
        insert_candidate(&conn, 3, 1024, 8);
        conn.execute("UPDATE files SET ftype = 'dir' WHERE id = 3", []).expect("arm dir");
        insert_candidate(&conn, 4, 1024, 8);
        conn.execute("UPDATE files SET sha1 = NULL WHERE id = 4", []).expect("arm null sha");
        insert_candidate(&conn, 5, 1024, 8);
        conn.execute(
            "UPDATE files SET sparse_count = NULL WHERE id = 5", [],
        ).expect("arm null sparse_count");
        insert_candidate(&conn, 6, 1024, 2);   // sparse_count < :min_pages
        insert_candidate(&conn, 7, 1024, 8);
        conn.execute(
            "UPDATE files SET include_reason_archive = 0 WHERE id = 7", [],
        ).expect("arm filter fail");
        insert_candidate(&conn, 8, 1024, 8);   // keeper: passes everything

        let n = promote_non_sparsify_candidates_to_sparsified(&conn, MIN_PAGES).expect("promote");

        assert_eq!(n, 7);
        for id in 1..8 {
            assert_eq!(row_phase(&conn, id), "sparsified");
        }
        assert_eq!(row_phase(&conn, 8), "deduped");
    }

    #[test]
    fn pull_skips_promoted_and_errored_rows() {
        let (_dir, conn) = open_db();
        insert_candidate(&conn, 1, 1024, 8);
        insert_candidate(&conn, 2, 1024, 8);
        insert_candidate(&conn, 3, 1024, 8);
        create_sparsify_queue(&conn).expect("create queue");
        populate_sparsify_queue(&conn, MIN_PAGES).expect("populate queue");

        set_phase(&conn, 2, "sparsified");                      // promoted last session
        set_flag(&conn, 3, FileFlag::ErrorWhileSparsify);       // errored last session

        let rows = pull_pending_sparsify_rows::<StrippedRecord>(&conn, 0, 100)
            .expect("pull");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, 1);          // queue position 1
        assert_eq!(rows[0].1.id.0, 1);     // only the pending candidate

        // Slice-walk from the returned position reproduces the (now empty) tail.
        let tail = pull_pending_sparsify_rows::<StrippedRecord>(&conn, rows[0].0, 100)
            .expect("pull tail");
        assert!(tail.is_empty());
    }

    #[test]
    fn pull_filters_has_sparse_and_errorwhilesparse() {
        let (_dir, conn) = open_db();
        insert_candidate(&conn, 1, 1024, 8);
        insert_candidate(&conn, 2, 1024, 8);
        insert_candidate(&conn, 3, 1024, 8);
        create_sparsify_queue(&conn).expect("create queue");
        populate_sparsify_queue(&conn, MIN_PAGES).expect("populate queue");

        set_flag(&conn, 1, FileFlag::HasSparse);          // sticky success flag
        set_flag(&conn, 2, FileFlag::ErrorWhileSparsify); // sticky error flag

        let rows = pull_pending_sparsify_rows::<StrippedRecord>(&conn, 0, 100)
            .expect("pull");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1.id.0, 3);
    }

    #[test]
    fn count_pending_matches_pull() {
        let (_dir, conn) = open_db();
        insert_candidate(&conn, 1, 1024, 8);
        insert_candidate(&conn, 2, 1024, 8);
        insert_candidate(&conn, 3, 1024, 8);
        set_phase(&conn, 2, "sparsified");
        set_flag(&conn, 3, FileFlag::HasSparse);
        create_sparsify_queue(&conn).expect("create queue");

        // 4th row inserted after the queue exists: populate must catch it too.
        insert_candidate(&conn, 4, 1024, 8);
        set_flag(&conn, 4, FileFlag::ErrorWhileSparsify);
        populate_sparsify_queue(&conn, MIN_PAGES).expect("populate queue");

        let rows = pull_pending_sparsify_rows::<StrippedRecord>(&conn, 0, 100)
            .expect("pull");
        assert_eq!(rows.len() as u64, 1);   // only candidate 1 is todo
        assert_eq!(
            count_pending_sparsify_candidates(&conn, MIN_PAGES).expect("count pending"),
            1
        );
    }

    #[test]
    fn count_all_is_workload_across_sessions() {
        let (_dir, mut conn) = open_db();
        insert_candidate(&conn, 1, 1024, 8);
        insert_candidate(&conn, 2, 1024, 8);
        insert_candidate(&conn, 3, 1024, 8);

        assert_eq!(
            count_all_sparsify_candidates(&conn, MIN_PAGES).expect("count all"),
            3
        );
        assert_eq!(
            count_pending_sparsify_candidates(&conn, MIN_PAGES).expect("count pending"),
            3
        );

        let mut batch = Vec::<SparseOutcome>::new();
        batch.push(SparseOutcome { id: FileId(1), modified: false, err: None });
        assert_eq!(ingest_sparsify_outcome(&mut conn, &batch).expect("ingest"), 1);

        assert_eq!(
            count_all_sparsify_candidates(&conn, MIN_PAGES).expect("count all stable"),
            3
        );
        assert_eq!(
            count_pending_sparsify_candidates(&conn, MIN_PAGES).expect("count pending"),
            2
        );

        let mut rest = Vec::<SparseOutcome>::new();
        rest.push(SparseOutcome { id: FileId(2), modified: false, err: None });
        rest.push(SparseOutcome { id: FileId(3), modified: false, err: None });
        assert_eq!(ingest_sparsify_outcome(&mut conn, &rest).expect("ingest rest"), 2);

        assert_eq!(
            count_all_sparsify_candidates(&conn, MIN_PAGES).expect("count all still 3"),
            3
        );
        assert_eq!(
            count_pending_sparsify_candidates(&conn, MIN_PAGES).expect("count pending 0"),
            0
        );
    }

    #[test]
    fn ingest_flag_and_phase_are_atomic() {
        let (_dir, mut conn) = open_db();
        insert_candidate(&conn, 1, 1024, 8);
        insert_candidate(&conn, 2, 1024, 8);
        insert_candidate(&conn, 3, 1024, 8);

        let mut batch = Vec::<SparseOutcome>::new();
        batch.push(SparseOutcome { id: FileId(1), modified: false, err: None });
        batch.push(SparseOutcome {
            id: FileId(2), modified: false, err: Some(file_stat_err()),
        });
        batch.push(SparseOutcome {
            id: FileId(3), modified: false, err: Some(Error::Interrupted),
        });

        let resolved = ingest_sparsify_outcome(&mut conn, &batch).expect("ingest");

        assert_eq!(resolved, 2);
        // HasSparse ⟺ sparsified; ErrorWhileSparsify ⟺ sparsified.
        assert_ne!(row_flags(&conn, 1) & FLAG_HAS_SPARSE, 0);
        assert_eq!(row_phase(&conn, 1), "sparsified");
        assert_eq!(row_flags(&conn, 2) & FLAG_HAS_SPARSE, 0);
        assert_ne!(row_flags(&conn, 2) & FLAG_ERR_SPARSIFY, 0);
        assert_eq!(row_phase(&conn, 2), "sparsified");
        // Interrupted stays untouched, no half-applied flag.
        assert_eq!(row_flags(&conn, 3), 0);
        assert_eq!(row_phase(&conn, 3), "deduped");
    }

    #[test]
    fn ingest_sets_modified_correctly() {
        let (_dir, mut conn) = open_db();
        insert_candidate(&conn, 1, 1024, 8);
        insert_candidate(&conn, 2, 1024, 8);
        insert_candidate(&conn, 3, 1024, 8);

        let mut batch = Vec::<SparseOutcome>::new();
        batch.push(SparseOutcome { id: FileId(1), modified: true, err: None });
        batch.push(SparseOutcome {
            id: FileId(2), modified: true, err: Some(file_stat_err()),
        });
        batch.push(SparseOutcome {
            id: FileId(3), modified: false, err: Some(file_stat_err()),
        });
        ingest_sparsify_outcome(&mut conn, &batch).expect("ingest");

        assert!(row_flags(&conn, 1) & FLAG_MODIFIED != 0);
        assert!(row_flags(&conn, 2) & FLAG_MODIFIED != 0);
        assert_eq!(row_flags(&conn, 3) & FLAG_MODIFIED, 0);
    }

    #[test]
    fn ingest_panics_on_invalid_error_variants() {
        let variants = [
            Error::Config("invalid config".into()),
            Error::Other(anyhow::anyhow!("boom")),
            Error::Database(rusqlite::Error::InvalidParameterName("boom".to_string())),
        ];
        for variant in variants {
            let (_dir, mut conn) = open_db();
            insert_candidate(&conn, 1, 1024, 8);
            let mut batch = Vec::<SparseOutcome>::new();
            batch.push(SparseOutcome { id: FileId(1), modified: false, err: Some(variant) });
            let res = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
                let _ = ingest_sparsify_outcome(&mut conn, &batch)?;
                Ok(())
            }));
            assert!(res.is_err());
        }
    }

    #[test]
    fn queue_populate_orders_by_size_desc() {
        let (_dir, conn) = open_db();
        insert_candidate(&conn, 1, 4 * 1024 * 1024, 8);   // biggest
        insert_candidate(&conn, 2, 4 * 1024 * 1024 + 1, 8);
        insert_candidate(&conn, 3, 1024 * 1024, 8);
        create_sparsify_queue(&conn).expect("create queue");
        populate_sparsify_queue(&conn, MIN_PAGES).expect("populate queue");

        let rows = pull_pending_sparsify_rows::<StrippedRecord>(&conn, 0, 100)
            .expect("pull");
        let got = rows
            .into_iter()
            .map(|pair| (pair.1.id.0, pair.0))
            .collect::<Vec<(i64, u64)>>();
        assert_eq!(got, [(2, 1), (1, 2), (3, 3)]);   // size DESC, positions 1..3
    }

    #[test]
    fn populate_is_idempotent() {
        let (_dir, conn) = open_db();
        insert_candidate(&conn, 1, 1024, 8);
        insert_candidate(&conn, 2, 1024, 8);
        create_sparsify_queue(&conn).expect("create queue");
        populate_sparsify_queue(&conn, MIN_PAGES).expect("populate 1");
        populate_sparsify_queue(&conn, MIN_PAGES).expect("populate 2");

        let rows = pull_pending_sparsify_rows::<StrippedRecord>(&conn, 0, 100)
            .expect("pull");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 1);
        assert_eq!(rows[1].0, 2);
    }
}
