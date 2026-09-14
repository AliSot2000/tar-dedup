use crate::db::SqlFileRow;
use crate::db::common::generate_archive_and_extract_filter;
use crate::db::flags::FileFlag;
use rusqlite::{Connection, named_params};

/// Promote every `unarchived` row to `rehashed` without verifying payloads.
pub fn skip_rehash(conn: &Connection) -> crate::error::Result<u64> {
    // INFO: Technically, there should not be any file that is not 'unarchived' or 'rehashed'
    let n = conn.execute(
        "UPDATE files SET phase = 'rehashed' WHERE phase = 'unarchived'",
        [],
    )?;
    Ok(n as u64)
}

pub fn list_files_to_rehash<R: SqlFileRow>(conn: &Connection, batch_size: u64)
    -> crate::error::Result<Vec<R>> {
    let cols = R::sql_columns(None);
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols} FROM files
            WHERE canonical_id = id
                AND flags & :extracted != 0
                AND {}
                AND ftype = 'file'
                AND phase = 'unarchived'
                LIMIT :batch_size",
        generate_archive_and_extract_filter(None)
    ))?;

    let rows = stmt.query_map(
        named_params! {
            ":extracted": FileFlag::FileExtracted.mask_i64(),
            ":batch_size": batch_size,
        },
        |row| R::from_row(row, None),
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}
