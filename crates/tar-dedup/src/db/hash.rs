use crate::db::SqlFileRow;
use crate::db::common::generate_archive_filter;
use crate::db::flags::{FileFlag, set_file_flag};
use crate::db::types::FileId;
use crate::error::{Error, Result, ToPanic};
use rusqlite::{Connection, named_params};


pub struct HashError {
    pub modified: bool,
    pub id: FileId,
    pub err: Error,
}

pub struct HashSuccess {
    pub modified: bool,
    pub id: FileId,
    pub zero_pages: u64,
    pub hash: [u8; 20],
}
pub type HashingOutcome = std::result::Result<HashSuccess, HashError>;


/// Shared WHERE selecting *all* files the hash phase will ever touch — the
/// stable set, independent of hashing progress (no `sha1` / error predicate).
/// Used by both `count_all_hashable_files` and `populate_hash_queue`.
fn hashable_files_where(eager_filter: bool, detect_hardlinks: bool) -> String {
    let phase = if eager_filter {
        "'filtered'"
    } else {
        "'inventoried'"
    };
    let filtered_selection = if eager_filter {
        format!("AND {}", generate_archive_filter(None))
    } else {
        String::new()
    };
    let filter_hardlink_canonical = if detect_hardlinks {
        "AND (flags & :flag) != 0"
    } else {
        ""
    };
    format!(
        "phase = {phase}
         AND ftype = 'file'
         {filter_hardlink_canonical}
         {filtered_selection}"
    )
}

/// Create the `hash_queue` ordering table. Idempotent.
pub fn create_hash_queue(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS hash_queue (
             id      INTEGER PRIMARY KEY,
             file_id INTEGER NOT NULL UNIQUE REFERENCES files(id)
         )",
        [],
    ).to_panic()?;
    Ok(())
}

/// Populate `hash_queue` with the full, stable set of hashable files in
/// `size DESC, id` order (position = `row_number()`). Idempotent: `INSERT OR
/// IGNORE` (via `UNIQUE(file_id)`) keeps rows from an earlier populate (e.g. a
/// resume) and adds only missing ones, so the queue is invariant to how much
/// hashing has already completed.
pub fn populate_hash_queue(conn: &Connection, eager_filter: bool, detect_hardlinks: bool)
    -> Result<u64> {
    let sql = format!(
        "INSERT OR IGNORE INTO hash_queue (id, file_id)
         SELECT row_number() OVER (ORDER BY size DESC, id), id
         FROM files WHERE {}",
        hashable_files_where(eager_filter, detect_hardlinks)
    );
    let n = if detect_hardlinks {
        conn.execute(&sql, named_params! { ":flag": FileFlag::FileHardlinkCanonical.mask_i64() })
            .to_panic()?
    } else {
        conn.execute(&sql, []).to_panic()?
    };
    Ok(n as u64)
}

/// Next slice of still-pending hash rows, in `hash_queue` (size-DESC) order.
///
/// Joins the queue against `files`, returning only rows not yet hashed
/// (`sha1 IS NULL`, no error flag), so a resume skips already-done files; the
/// queue only supplies the ordering. The returned `u64` is the queue position
/// of each row, letting the caller advance the read `index` across slices.
/// O(n) overall on the queue PK + files PK join. rusqlite statements never
/// outlive this call, so all SQLite stays inside `db/` (a long-lived lazy
/// cursor is not expressible: a `Statement` borrows its `Connection`).
pub fn pull_pending_hash_rows<R: SqlFileRow>(
    conn: &Connection,
    index: u64,
    limit: u64,
) -> Result<Vec<(u64, R)>> {
    let cols = R::sql_columns(Some("files"));
    let sql = format!(
        "SELECT hash_queue.id AS pos, {cols}
         FROM files JOIN hash_queue ON hash_queue.file_id = files.id
         WHERE hash_queue.id > :index
           AND files.sha1 IS NULL
           AND (files.flags & :sha_error) = 0
         ORDER BY hash_queue.id
         LIMIT :limit"
    );
    let mut stmt = conn.prepare(&sql).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":index": index as i64,
            ":sha_error": FileFlag::ErrorWhileHash.mask_i64(),
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

/// Drop the `hash_queue` ordering table. Idempotent. Call on the phase-success
/// path only; leave it in place across interrupts/resumes so a repopulate is a
/// no-op for already-queued rows.
pub fn drop_hash_queue(conn: &Connection) -> Result<()> {
    conn.execute("DROP TABLE IF EXISTS hash_queue", []).to_panic()?;
    Ok(())
}

/// Count all rows that need to be hashed in this phase. Not only the remaining files.
pub fn promote_unhasheable_files(conn: &Connection, eager_filter: bool, detect_hardlinks: bool)
    -> Result<u64> {
    let phase = if eager_filter {
        "'filtered'"
    } else {
        "'inventoried'"
    };
    let filtered_selection = if eager_filter {
        format!("OR NOT ({})", generate_archive_filter(None))
    } else {
        String::new()
    };
    let filter_hardlink_canonical = if detect_hardlinks {
        "OR (flags & :hardlink) == 0"
    } else {
        ""
    };
    let sql = format!(
        "UPDATE files SET phase = 'hashed' WHERE phase = {phase}
             AND (ftype != 'file'
                   {filtered_selection}
                   {filter_hardlink_canonical}) "
    );
    let params = if detect_hardlinks {
        named_params! {
            ":hardlink": FileFlag::FileHardlinkCanonical.mask_i64(),
        }
    } else { named_params!{} };
    let count  = conn.execute(&sql, params).to_panic()?;
    Ok(count as u64)
}

/// Count all rows that remain to be hashed in this phase.
pub fn count_pending_hashable_files(conn: &Connection, eager_filter: bool, detect_hardlinks: bool)
    -> Result<u64> {
    let phase = if eager_filter {
        "'filtered'"
    } else {
        "'inventoried'"
    };
    let filtered_selection = if eager_filter {
        format!("AND {}", generate_archive_filter(None))
    } else {
        String::new()
    };
    let filter_hardlink_canonical = if detect_hardlinks {
        "AND (flags & :hardlink) != 0"
    } else {
        ""
    };
    let sql = format!(
        "SELECT COUNT(*) AS count
         FROM files
         WHERE phase = {phase}
             AND ftype = 'file'
             AND (flags & :sha_error) = 0
             AND sha1 IS NULL
             {filter_hardlink_canonical}
             {filtered_selection}"
    );
    let params = if detect_hardlinks {
        named_params! {
            ":hardlink": FileFlag::FileHardlinkCanonical.mask_i64(),
            ":sha_error": FileFlag::ErrorWhileHash.mask_i64()
        }
    } else {
        named_params! {
            ":sha_error": FileFlag::ErrorWhileHash.mask_i64()
        }
    };
    let count: u64  = conn.query_row(&sql, params, |r| r.get("count")).to_panic()?;
    Ok(count)
}

/// Count all rows that need to be hashed in this phase. Not only the remaining files.
pub fn count_all_hashable_files(conn: &Connection, eager_filter: bool, detect_hardlinks: bool)
    -> Result<u64> {
    let sql = format!(
        "SELECT COUNT(*) AS count FROM files WHERE {}",
        hashable_files_where(eager_filter, detect_hardlinks)
    );
    let count: i64 = if detect_hardlinks {
        conn.query_row(&sql, named_params! {
            ":flag": FileFlag::FileHardlinkCanonical.mask_i64(),
        },
       |row| row.get("count")).to_panic()?
    } else {
        conn.query_row(&sql, [], |row| row.get("count")).to_panic()?
    };
    Ok(count as u64)
}

pub fn update_file_inspection_per_id(
    conn: &Connection, file_id: FileId, digest: [u8; 20], sparse_count: u64, update_hardlinks: bool)
    -> Result<()> {
    let sql = if update_hardlinks {
        "UPDATE files SET sha1 = :sha1, sparse_count = :sparse_count, phase = 'hashed'
            WHERE (dev, inode) IN (SELECT dev, inode FROM files WHERE id = :id)"
    } else {
        "UPDATE files SET sha1 = :sha1, sparse_count = :sparse_count, phase = 'hashed'
         WHERE id = :id"
    };
    conn.execute(
        sql,
        named_params! {
            ":sha1": digest.as_slice(),
            ":sparse_count": sparse_count as i64,
            ":id": file_id.0,
        },
    ).to_panic()?;
    Ok(())
}

pub fn ingest_hash_outcome(
    conn: &mut Connection, results: &Vec<HashingOutcome>, update_hardlinks: bool)
    -> Result<u64> {
    let tx = conn.transaction().to_panic()?;
    for outcome in results.iter() {
        match outcome {
            Ok(hs ) => {
                if hs.modified {
                    set_file_flag(&tx, hs.id, FileFlag::Modified, true)?;
                }
                update_file_inspection_per_id(
                    &tx, hs.id, hs.hash, hs.zero_pages, update_hardlinks
                )?;
            }
            Err(he ) => {
                if he.modified {
                    set_file_flag(&tx, he.id, FileFlag::Modified, true)?;
                }
                match &he.err {
                    Error::Interrupted => (),
                    Error::FileStat(_) => {
                        let _ = set_file_flag(&tx, he.id, FileFlag::ErrorWhileHash, true)?;
                    },
                    other => panic!(
                        "Invariant Error. Only FileStatError and Interrupted expected. Got: {other}"
                    ),
                }
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
    use crate::error::{Error, FileStatError, Result};
    use std::panic::AssertUnwindSafe;
    use std::panic::catch_unwind;
    use std::path::PathBuf;

    // FileFlag bit masks (bits 5/6) — literals keep constants const-callable.
    const FLAG_MODIFIED: i64 = 1i64 << 5;
    const FLAG_SHA_ERROR: i64 = 1i64 << 6;

    fn open_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = Connection::open(&dir.path().join("t.sqlite")).expect("open conn");
        schema::initialize(&conn).expect("schema init");
        (dir, conn)
    }

    fn insert_row(conn: &Connection, id: i64, abs_path: &str, dev: i64, inode: i64) {
        conn.execute(
            "INSERT INTO files (id, abs_path, ext, size, ftype, phase, dev, inode) \
             VALUES (:id, :abs_path, '.bin', 0, 'file', 'inventoried', :dev, :inode)",
            named_params! {
                ":id": id,
                ":abs_path": abs_path,
                ":dev": dev,
                ":inode": inode,
            },
        ).expect("insert row");
    }

    fn insert_default_row(conn: &Connection, id: i64) {
        let abs_path = format!("/tmp/file{id}.bin");
        insert_row(conn, id, abs_path.as_str(), 777, id * 1000);
    }

    fn row_flags(conn: &Connection, id: i64) -> i64 {
        conn.query_row(
            "SELECT flags FROM files WHERE id = :id",
            named_params! { ":id": id },
            |row| row.get(0),
        ).expect("read flags")
    }

    fn row_sha1(conn: &Connection, id: i64) -> Option<Vec<u8>> {
        conn.query_row(
            "SELECT sha1 FROM files WHERE id = :id",
            named_params! { ":id": id },
            |row| row.get::<_, Option<Vec<u8>>>(0),
        ).expect("read sha1")
    }

    fn row_phase(conn: &Connection, id: i64) -> String {
        conn.query_row(
            "SELECT phase FROM files WHERE id = :id",
            named_params! { ":id": id },
            |row| row.get::<_, String>(0),
        ).expect("read phase")
    }

    #[test]
    fn ingest_writes_digest_phase_sparse_count() {
        let (_dir, mut conn) = open_db();
        insert_default_row(&conn, 1);
        let digest: [u8; 20] = [7u8; 20];
        let mut outcomes = Vec::<HashingOutcome>::new();
        outcomes.push(Ok(HashSuccess {
            id: FileId(1),
            modified: false,
            zero_pages: 7,
            hash: digest,
        }));

        let applied = ingest_hash_outcome(&mut conn, &outcomes, false).expect("ingest");

        assert_eq!(applied, 0);
        assert_eq!(row_flags(&conn, 1) & FLAG_MODIFIED, 0);
        assert_eq!(row_sha1(&conn, 1), Some(digest.as_slice().to_vec()));
        assert_eq!(
            conn.query_row(
                "SELECT sparse_count FROM files WHERE id = 1", [], |row| row.get::<_, i64>(0)
            ).expect("sparse"),
            7
        );
        assert_eq!(row_phase(&conn, 1), "hashed");
    }

    #[test]
    fn ingest_sets_modified_flag_on_ok_and_err() {
        let (_dir, mut conn) = open_db();
        insert_default_row(&conn, 1);
        insert_default_row(&conn, 2);
        let digest: [u8; 20] = [9u8; 20];
        let mut outcomes = Vec::<HashingOutcome>::new();
        outcomes.push(Ok(HashSuccess {
            id: FileId(1), modified: true, zero_pages: 0, hash: digest,
        }));
        outcomes.push(Err(HashError {
            id: FileId(2),
            modified: true,
            err: Error::FileStat(FileStatError::General {
                path: None,
                message: "boom".to_string(),
            }),
        }));

        ingest_hash_outcome(&mut conn, &outcomes, false).expect("ingest");

        assert_ne!(row_flags(&conn, 1) & FLAG_MODIFIED, 0);
        assert_ne!(row_flags(&conn, 2) & FLAG_MODIFIED, 0);
    }

    #[test]
    fn ingest_filestat_error_sets_error_while_hash() {
        let (_dir, mut conn) = open_db();
        insert_default_row(&conn, 1);
        let mut outcomes = Vec::<HashingOutcome>::new();
        outcomes.push(Err(HashError {
            id: FileId(1),
            modified: false,
            err: Error::FileStat(FileStatError::Io {
                path: PathBuf::from("/tmp/nope.bin"),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied, "denied".to_string(),
                ),
            }),
        }));

        ingest_hash_outcome(&mut conn, &outcomes, false).expect("ingest");

        assert_ne!(row_flags(&conn, 1) & FLAG_SHA_ERROR, 0);
        assert_eq!(row_flags(&conn, 1) & FLAG_MODIFIED, 0);
        if let Some(_) = row_sha1(&conn, 1) {
            panic!("FileStat error must not store a digest");
        }
        assert_eq!(row_phase(&conn, 1), "inventoried");
    }

    #[test]
    fn ingest_discards_interrupted_untouched() {
        let (_dir, mut conn) = open_db();
        insert_default_row(&conn, 1);
        let mut outcomes = Vec::<HashingOutcome>::new();
        outcomes.push(Err(HashError {
            id: FileId(1),
            modified: false,
            err: Error::Interrupted,
        }));

        ingest_hash_outcome(&mut conn, &outcomes, false).expect("ingest");

        assert_eq!(row_flags(&conn, 1), 0);
        assert_eq!(row_phase(&conn, 1), "inventoried");
        if let Some(_) = row_sha1(&conn, 1) {
            panic!("Interrupted outcome must not store a digest");
        }
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
            insert_default_row(&conn, 1);
            let mut outcomes = Vec::<HashingOutcome>::new();
            outcomes.push(Err(HashError {
                id: FileId(1),
                modified: false,
                err: variant,
            }));
            let res = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
                let _ = ingest_hash_outcome(&mut conn, &outcomes, false)?;
                Ok(())
            }));
            assert!(res.is_err());
        }
    }

    #[test]
    fn update_file_inspection_propagates_hardlink_group() {
        let (_dir, conn) = open_db();
        insert_row(&conn, 1, "/tmp/a.bin", 5, 10);
        insert_row(&conn, 2, "/tmp/b.bin", 5, 10);
        let digest: [u8; 20] = [3u8; 20];

        update_file_inspection_per_id(&conn, FileId(1), digest, 3, true).expect("update");

        assert_eq!(row_sha1(&conn, 1), Some(digest.as_slice().to_vec()));
        assert_eq!(row_sha1(&conn, 2), Some(digest.as_slice().to_vec()));
        assert_eq!(
            conn.query_row(
                "SELECT sparse_count FROM files WHERE id = 2", [], |row| row.get::<_, i64>(0)
            ).expect("sparse"),
            3
        );
        assert_eq!(row_phase(&conn, 2), "hashed");
        assert_eq!(
            conn.query_row(
                "SELECT sparse_count FROM files WHERE id = 1", [], |row| row.get::<_, i64>(0)
            ).expect("sparse"),
            3
        );
        assert_eq!(row_phase(&conn, 1), "hashed");
    }

    #[test]
    fn update_file_inspection_single_row_without_hardlinks() {
        let (_dir, conn) = open_db();
        insert_row(&conn, 1, "/tmp/a.bin", 5, 10);
        insert_row(&conn, 2, "/tmp/b.bin", 5, 10);
        let digest: [u8; 20] = [3u8; 20];

        update_file_inspection_per_id(&conn, FileId(1), digest, 1, false).expect("update");

        assert_eq!(row_sha1(&conn, 1), Some(digest.as_slice().to_vec()));
        assert_eq!(row_phase(&conn, 1), "hashed");
        if let Some(_) = row_sha1(&conn, 2) {
            panic!("without hardlink propagation the sibling row must stay untouched");
        }
        assert_eq!(row_phase(&conn, 2), "inventoried");
    }
}