use crate::db::SqlFileRow;
use crate::db::common::generate_archive_filter;
use crate::db::flags::FileFlag;
use crate::db::types::FileId;
use crate::error::{Result, ToPanic};
use rusqlite::{Connection, named_params};


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

/// Get all files that still need to be inspected
pub fn get_entries_to_hash<R: SqlFileRow>(
    conn: &Connection, eager_filter: bool, detect_hardlinks: bool, batch_size: u64)
    -> Result<Vec<R>> {
    let cols = R::sql_columns(None);
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
    let sql = format!(
        "SELECT {cols} FROM files WHERE phase = {phase}
             AND (flags & :sha_error) = 0
             AND sha1 IS NULL
             AND ftype = 'file'
             {filter_hardlink_canonical}
             {filtered_selection}
             ORDER BY size DESC
             LIMIT :batch_size"
    );
    let mut stmt = conn.prepare(&sql).to_panic()?;
    let row_mapper = |r: &rusqlite::Row<'_>| R::from_row(r, None);

    let rows = if detect_hardlinks {
        stmt.query_map(named_params! {
            ":sha_error": FileFlag::ErrorWhileHash.mask_i64(),
            ":flag": FileFlag::FileHardlinkCanonical.mask_i64(),
            ":batch_size": batch_size,
        },
        row_mapper).to_panic()?
    } else {
        stmt.query_map(
            named_params! {
                ":sha_error": FileFlag::ErrorWhileHash.mask_i64(),
                ":batch_size": batch_size,
            },
            row_mapper,
        ).to_panic()?
    };
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
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
