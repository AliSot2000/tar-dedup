use crate::db::SqlFileRow;
use crate::db::flags::FileFlag;
use crate::db::types::FileId;
use crate::error::Result;
use rusqlite::{Connection, named_params};

/// Get all files that still need to be inspected
pub fn get_entries_to_hash<R: SqlFileRow>(
    conn: &Connection, eager_filter: bool, detect_hardlinks: bool) -> Result<Vec<R>> {
    let cols = R::sql_columns();
    let phase = if eager_filter { "'filtered'" } else { "'inventoried'" };
    let filtered_selection = if eager_filter {
        "AND include_reason < 0 \
         AND exclude_reason = 0"
    } else {
        ""
    };
    let filter_hardlink_canonical = if detect_hardlinks {
        "AND (flags & :flag) != 0"
    } else {
        ""
    };
    let sql = format!(
        "SELECT {cols} FROM files WHERE phase = {phase} \
             AND (flags & :sha_error) = 0 \
             AND sha1 IS NOT NULL \
             AND ftype = 'file' \
             {filter_hardlink_canonical} \
             {filtered_selection}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = if detect_hardlinks {
        stmt.query_map(named_params! {
            ":sha_error": FileFlag::ErrorWhileHash.mask_i64(),
            ":flag": FileFlag::FileHardlinkCanonical.mask_i64(),
        },
        R::from_row)?
    } else {
        stmt.query_map(
            named_params! { ":sha_error": FileFlag::ErrorWhileHash.mask_i64() },
            R::from_row
        )?
    };
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Count all rows that need to be hashed in this phase. Not only the remaining files.
pub fn count_all_hashable_files(
    conn: &Connection, eager_filter: bool, detect_hardlinks: bool) -> Result<u64> {
    let phase = if eager_filter { "'filtered'" } else { "'inventoried'" };
    let filtered_selection = if eager_filter {
        "AND include_reason < 0 \
         AND exclude_reason = 0"
    } else {
        ""
    };
    let filter_hardlink_canonical = if detect_hardlinks {
        "AND (flags & :flag) != 0"
    } else {
        ""
    };
    let sql = format!(
        "SELECT COUNT(*) AS count FROM files WHERE phase = {phase} \
             AND ftype = 'file' \
             {filter_hardlink_canonical} \
             {filtered_selection}"
    );
    let count: i64 = if detect_hardlinks {
        conn.query_row(&sql, named_params! {
            ":flag": FileFlag::FileHardlinkCanonical.mask_i64(),
        },
        |row| row.get("count"))?
    } else {
        conn.query_row(&sql, [], |row| row.get("count"))?
    };
    Ok(count as u64)
}

pub fn update_file_inspection_per_id(
    conn: &Connection,
    file_id: FileId,
    digest: [u8; 20],
    sparse_count: u64,
    update_hardlinks: bool
) -> Result<()> {
    let sql = if update_hardlinks {
        "UPDATE files SET sha1 = :sha1, sparse_count = :sparse_count, phase = 'hashed' \
            WHERE (dev, inode) IN (SELECT dev, inode FROM files WHERE id = :id)"
    } else {
        "UPDATE files SET sha1 = :sha1, sparse_count = :sparse_count, phase = 'hashed' \
         WHERE id = :id"
    };
    conn.execute(
        sql,
        named_params! {
            ":sha1": digest.as_slice(),
            ":sparse_count": sparse_count as i64,
            ":id": file_id.0,
        },
    )?;
    Ok(())
}
