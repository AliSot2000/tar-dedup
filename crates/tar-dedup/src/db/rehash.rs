use crate::db::SqlFileRow;
use crate::db::common::generate_archive_and_extract_filter;
use crate::db::common::mark_phase;
use crate::db::flags::{FileFlag, set_file_flag};
use crate::db::types::{FileId, FilePhase};
use crate::error::{FileStatError, Result, ToPanic};
use rusqlite::{Connection, named_params};

/// Promote every `extract_filtered` row to `rehashed` without verifying payloads.
pub fn skip_rehash(conn: &Connection) -> Result<u64> {
    // INFO: Technically, there should not be any file that is not 'extract_filtered' or 'rehashed'
    let n = conn.execute(
        "UPDATE files SET phase = 'rehashed' WHERE phase = 'extract_filtered'",
        [],
    ).to_panic()?;
    Ok(n as u64)
}

/// Shared WHERE selecting *all* rows the rehash phase verifies — the stable set,
/// independent of rehash progress (no phase predicate). Self-canonical regular
/// files that carry `FileExtracted`, passed the joint archive+extract filter, and
/// hold a digest to compare against. Used by both `count_files_to_rehash` and
/// `populate_rehash_queue`. Param: `:extracted`.
fn files_to_rehash_where() -> String {
    format!(
        "canonical_id = id
         AND ftype = 'file'
         AND sha1 IS NOT NULL
         AND (flags & :extracted) != 0
         AND {}",
        generate_archive_and_extract_filter(None)
    )
}

/// Count the **overall rehash workload** — elected rows regardless of phase.
/// Stable across sessions, so a resumed run's phase bar still reflects the full
/// workload (mirrors `db/hash.rs::count_all_hashable_files`).
pub fn count_files_to_rehash(conn: &Connection) -> Result<u64> {
    let sql = format!(
        "SELECT COUNT(*) AS count FROM files WHERE {}",
        files_to_rehash_where()
    );
    let count: i64 = conn.query_row(
        &sql,
        named_params! {
            ":extracted": FileFlag::FileExtracted.mask_i64(),
        },
        |row| row.get("count"),
    ).to_panic()?;
    Ok(count as u64)
}

/// Of `count_files_to_rehash`, how many are already `rehashed` — the resume
/// position on the phase bar (`pending` is the difference).
pub fn count_rehashed_files(conn: &Connection) -> Result<u64> {
    let sql = format!(
        "SELECT COUNT(*) AS count FROM files
         WHERE {} AND phase = 'rehashed'",
        files_to_rehash_where()
    );
    let count: i64 = conn.query_row(
        &sql,
        named_params! {
            ":extracted": FileFlag::FileExtracted.mask_i64(),
        },
        |row| row.get("count"),
    ).to_panic()?;
    Ok(count as u64)
}

/// Advance every `extract_filtered` row that is **not** elected (dupes,
/// non-files, filter-excluded, sha-less) straight to `rehashed`, so only elected
/// rows reach the queue. Null-safe negation (mirrors
/// `db/sparsify.rs::promote_non_sparsify_candidates_to_sparsified`).
pub fn promote_unrehashable_files(conn: &Connection) -> Result<u64> {
    let filtered = generate_archive_and_extract_filter(None);
    let n = conn.execute(&format!(
        "UPDATE files SET phase = 'rehashed'
         WHERE phase = 'extract_filtered'
           AND (
             canonical_id IS NULL
             OR canonical_id != id
             OR ftype != 'file'
             OR sha1 IS NULL
             OR (flags & :extracted) = 0
             OR NOT ({filtered})
         )"),
        named_params! {
            ":extracted": FileFlag::FileExtracted.mask_i64(),
        },
    ).to_panic()?;
    Ok(n as u64)
}

/// Create the `rehash_queue` ordering table. Idempotent.
pub fn create_rehash_queue(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS rehash_queue (
             id      INTEGER PRIMARY KEY,
             file_id INTEGER NOT NULL UNIQUE REFERENCES files(id)
         )",
        [],
    ).to_panic()?;
    Ok(())
}

/// Populate `rehash_queue` with the full, stable set of elected rows in
/// `size DESC, id` order (position = `row_number()`). Idempotent: `INSERT OR
/// IGNORE` (via `UNIQUE(file_id)`) keeps rows from an earlier populate (e.g. a
/// resume) and adds only missing ones.
pub fn populate_rehash_queue(conn: &Connection) -> Result<u64> {
    let sql = format!(
        "INSERT OR IGNORE INTO rehash_queue (id, file_id)
         SELECT row_number() OVER (ORDER BY size DESC, id), id
         FROM files WHERE {}",
        files_to_rehash_where()
    );
    let n = conn.execute(
        &sql,
        named_params! {
            ":extracted": FileFlag::FileExtracted.mask_i64(),
        },
    ).to_panic()?;
    Ok(n as u64)
}

/// Next slice of still-pending rehash rows, in `rehash_queue` (size-DESC) order.
///
/// Joins the queue against `files`, returning only rows not yet rehashed
/// (`phase = 'extract_filtered'`), so a resume skips already-done rows; the queue
/// only supplies the ordering. The returned `u64` is the queue position of each
/// row, letting the caller advance the read `index` across slices. Same cursor
/// contract as the hash queue; see `db/hash.rs`.
///
/// The predicate is deliberately **phase-only**: `ingest_rehash_outcome` sets any
/// sticky flag (`RehashMismatch` / `ErrorWhileRehashing`) *alongside*
/// `phase = 'rehashed'`, so an errored row is already excluded by its phase.
/// Revisit only if a retry loop is observed.
pub fn pull_pending_rehash_rows<R: SqlFileRow>(
    conn: &Connection,
    index: u64,
    limit: u64,
) -> Result<Vec<(u64, R)>> {
    let cols = R::sql_columns(Some("files"));
    let sql = format!(
        "SELECT rehash_queue.id AS pos, {cols}
         FROM files JOIN rehash_queue ON rehash_queue.file_id = files.id
         WHERE rehash_queue.id > :index
           AND files.phase = 'extract_filtered'
         ORDER BY rehash_queue.id
         LIMIT :limit"
    );
    let mut stmt = conn.prepare(&sql).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":index": index as i64,
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

/// Drop the `rehash_queue` ordering table. Idempotent. Call on the phase-success
/// path only; leave it in place across interrupts/resumes so a repopulate is a
/// no-op for already-queued rows.
pub fn drop_rehash_queue(conn: &Connection) -> Result<()> {
    conn.execute("DROP TABLE IF EXISTS rehash_queue", []).to_panic()?;
    Ok(())
}

/// Result of a rehash worker run for one file.
#[derive(Debug)]
pub enum RehashOutcome {
    /// Digest matches catalog `sha1` (or no payload to verify — duplicate row).
    Match(FileId),
    /// Digest differs from catalog `sha1`.
    Mismatch(FileId),
    /// Rehash failed; carries the file id, message and (best-effort) cache path.
    Errored(FileId, FileStatError),
}

/// Apply one batch of worker outcomes in a single transaction. Every variant
/// advances the row to `rehashed`; `Mismatch` additionally sets `RehashMismatch`
/// and `Error`/`Failed` set `ErrorWhileRehashing`. The persistent error-log rows
/// for `Error`/`Failed` are recorded caller-side (a `Recorder`), mirroring
/// `archive/hash.rs::record_hash_error`.
pub fn ingest_rehash_outcome(
    conn: &mut Connection,
    results: &Vec<RehashOutcome>)
    -> Result<u64> {
    let tx = conn.transaction().to_panic()?;
    for outcome in results.iter() {
        match outcome {
            RehashOutcome::Match(id) => {
                mark_phase(&tx, *id, FilePhase::Rehashed)?;
            }
            RehashOutcome::Mismatch(id) => {
                assert_eq!(1, set_file_flag(&tx, *id, FileFlag::RehashMismatch, true)?);
                mark_phase(&tx, *id, FilePhase::Rehashed)?;
            }
            RehashOutcome::Errored(id, _fse) => {
                assert_eq!(1, set_file_flag(&tx, *id, FileFlag::ErrorWhileRehashing, true)?);
                mark_phase(&tx, *id, FilePhase::Rehashed)?;
            }
        }
    }
    tx.commit().to_panic()?;
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::db::types::StrippedRecord;
    use crate::error::FileStatError;
    use std::path::PathBuf;

    fn open_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = Connection::open(&dir.path().join("t.sqlite")).expect("open conn");
        schema::initialize(&conn).expect("schema init");
        // The joint archive+extract include rule sits at id -1 in *both* tables
        // (see generate_archive_and_extract_filter); without it rows fail the
        // filter and `promote_unrehashable_files` sweeps them unread. An id-5
        // exclude rule lets a test break the filter without tripping the FK.
        for table in ["archive", "extract"] {
            conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO filter_reason_{table} (id, source, line, expression) \
                     VALUES (-1, 'internal', NULL, '.*')"
                ),
                [],
            ).expect("seed internal include rule");
            conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO filter_reason_{table} (id, source, line, expression) \
                     VALUES (5, 'internal', NULL, 'exclude-test')"
                ),
                [],
            ).expect("seed internal exclude rule");
        }
        (dir, conn)
    }

    /// A `files` row in `extract_filtered` with the joint filter passed; the
    /// boolean knobs toggle exactly one failing `promote_unrehashable_files` arm.
    fn insert_row(
        conn: &Connection, id: i64, size: u64, ftype: &str,
        sha1: Option<[u8; 20]>, canonical: Option<i64>,
        extracted_flag: bool, filter_ok: bool) {
        let sha_bind = sha1.map(|s| s.to_vec());
        conn.execute(
            "INSERT INTO files (id, abs_path, ext, size, ftype, phase, sha1, \
             include_reason_archive, exclude_reason_archive, \
             include_reason_extract, exclude_reason_extract, flags, canonical_id) \
             VALUES (:id, :abs_path, '.bin', :size, :ftype, 'extract_filtered', :sha1, \
                     :inc_a, :exc_a, :inc_e, :exc_e, :flags, :canon)",
            named_params! {
                ":id": id,
                ":abs_path": format!("/tmp/re-{id}.bin"),
                ":size": size as i64,
                ":ftype": ftype,
                ":sha1": sha_bind.as_deref(),
                ":inc_a": -1,
                ":exc_a": 0,
                ":inc_e": -1,
                ":exc_e": if filter_ok { 0 } else { 5 },
                ":flags": if extracted_flag { FileFlag::FileExtracted.mask_i64() } else { 0 },
                ":canon": canonical,
            },
        ).expect("insert row");
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

    #[test]
    fn counts_stable_across_phase() {
        let (_dir, conn) = open_db();
        insert_row(&conn, 1, 1024, "file", Some([1u8; 20]), Some(1), true, true);
        insert_row(&conn, 2, 2048, "file", Some([2u8; 20]), Some(2), true, true);

        assert_eq!(count_files_to_rehash(&conn).expect("count"), 2);
        assert_eq!(count_rehashed_files(&conn).expect("done"), 0);

        mark_phase(&conn, FileId(1), FilePhase::Rehashed).expect("mark");

        assert_eq!(count_files_to_rehash(&conn).expect("stable"), 2);
        assert_eq!(count_rehashed_files(&conn).expect("done"), 1);
    }

    #[test]
    fn promote_covers_every_or_arm() {
        let (_dir, conn) = open_db();
        insert_row(&conn, 1, 1024, "file", Some([1u8; 20]), Some(1), true, true);   // keeper
        insert_row(&conn, 2, 1024, "file", Some([1u8; 20]), None, true, true);      // canonical NULL
        insert_row(&conn, 3, 1024, "file", Some([1u8; 20]), Some(1), true, true);   // canonical != id
        insert_row(&conn, 4, 1024, "dir", Some([1u8; 20]), Some(4), true, true);    // ftype != file
        insert_row(&conn, 5, 1024, "file", None, Some(5), true, true);              // sha1 NULL
        insert_row(&conn, 6, 1024, "file", Some([1u8; 20]), Some(6), false, true);  // extracted flag unset
        insert_row(&conn, 7, 1024, "file", Some([1u8; 20]), Some(7), true, false);  // filter fail

        let n = promote_unrehashable_files(&conn).expect("promote");

        assert_eq!(n, 6);
        assert_eq!(row_phase(&conn, 1), "extract_filtered");
        for id in 2..=7 {
            assert_eq!(row_phase(&conn, id), "rehashed", "id {id} should be promoted");
        }
    }

    #[test]
    fn queue_populate_orders_size_desc() {
        let (_dir, conn) = open_db();
        insert_row(&conn, 1, 4 * 1024 * 1024, "file", Some([1u8; 20]), Some(1), true, true);
        insert_row(&conn, 2, 4 * 1024 * 1024 + 1, "file", Some([2u8; 20]), Some(2), true, true);
        insert_row(&conn, 3, 1024 * 1024, "file", Some([3u8; 20]), Some(3), true, true);

        create_rehash_queue(&conn).expect("create queue");
        populate_rehash_queue(&conn).expect("populate 1");
        populate_rehash_queue(&conn).expect("populate 2");   // idempotent

        let got = pull_pending_rehash_rows::<StrippedRecord>(&conn, 0, 100)
            .expect("pull")
            .into_iter()
            .map(|(pos, row)| (row.id.0, pos))
            .collect::<Vec<(i64, u64)>>();
        assert_eq!(got, [(2, 1), (1, 2), (3, 3)]);
    }

    #[test]
    fn pull_skips_rehashed_rows() {
        let (_dir, conn) = open_db();
        // id2 is the biggest → queue position 1; once rehashed the pull skips it.
        insert_row(&conn, 1, 1024 * 1024, "file", Some([1u8; 20]), Some(1), true, true);
        insert_row(&conn, 2, 8 * 1024 * 1024, "file", Some([2u8; 20]), Some(2), true, true);
        insert_row(&conn, 3, 2 * 1024 * 1024, "file", Some([3u8; 20]), Some(3), true, true);
        create_rehash_queue(&conn).expect("create queue");
        populate_rehash_queue(&conn).expect("populate queue");
        mark_phase(&conn, FileId(2), FilePhase::Rehashed).expect("mark rehashed");

        let got = pull_pending_rehash_rows::<StrippedRecord>(&conn, 0, 100)
            .expect("pull")
            .into_iter()
            .map(|(pos, row)| (row.id.0, pos))
            .collect::<Vec<(i64, u64)>>();
        assert_eq!(got, [(3, 2), (1, 3)]);

        // slice-walk from the last returned position reproduces the empty tail
        let tail = pull_pending_rehash_rows::<StrippedRecord>(&conn, 3, 100)
            .expect("tail pull").len();
        assert_eq!(tail, 0);
    }

    #[test]
    fn ingest_flags_and_phase() {
        let (_dir, mut conn) = open_db();
        for id in 1..=3 {
            insert_row(&conn, id, 1024, "file", Some([id as u8; 20]), Some(id), true, true);
        }
        let err = FileStatError::Io {
            path: PathBuf::from("/tmp/re-3.bin"),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "nope"),
        };
        ingest_rehash_outcome(&mut conn, &vec![
            RehashOutcome::Match(FileId(1)),
            RehashOutcome::Mismatch(FileId(2)),
            RehashOutcome::Errored(FileId(3), err),
        ]).expect("ingest");

        for id in 1..=3 {
            assert_eq!(row_phase(&conn, id), "rehashed");
        }
        let flags = FileFlag::RehashMismatch.mask_i64();
        let err_flag = FileFlag::ErrorWhileRehashing.mask_i64();
        assert_eq!(row_flags(&conn, 1) & (flags | err_flag), 0);
        assert_ne!(row_flags(&conn, 2) & flags, 0);
        assert_eq!(row_flags(&conn, 2) & err_flag, 0);
        assert_eq!(row_flags(&conn, 3) & flags, 0);
        assert_ne!(row_flags(&conn, 3) & err_flag, 0);
    }

    #[test]
    fn skip_rehash_promotes_extract_filtered_only() {
        let (_dir, conn) = open_db();
        insert_row(&conn, 1, 1024, "file", Some([1u8; 20]), Some(1), true, true);
        insert_row(&conn, 2, 1024, "file", Some([2u8; 20]), Some(2), true, true);
        insert_row(&conn, 3, 1024, "file", Some([3u8; 20]), Some(3), true, true);
        conn.execute(
            "UPDATE files SET phase = 'unarchived' WHERE id = 2",
            [],
        ).expect("phase unarchived");
        mark_phase(&conn, FileId(3), FilePhase::Rehashed).expect("already rehashed");

        let n = skip_rehash(&conn).expect("skip");

        assert_eq!(n, 1);
        assert_eq!(row_phase(&conn, 1), "rehashed");
        assert_eq!(row_phase(&conn, 2), "unarchived");
        assert_eq!(row_phase(&conn, 3), "rehashed");
    }
}
