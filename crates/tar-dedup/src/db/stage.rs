use rusqlite::Connection;

use crate::db::common::SqlFileRow;
use crate::error::Result;

pub fn promote_unstageable_files(conn: &Connection, retry_missing_sha: bool)
    -> Result<u64> {
    let filter_sha = if retry_missing_sha { "" } else { "OR sha1 IS NULL" };
    let n = conn.execute(&format!(
        "UPDATE files SET phase = 'staged' \
        WHERE phase = 'sparsified' \
        AND ( \
            ftype IS NULL \
            OR ftype != 'file' \
            OR canonical_id IS NULL \
            OR canonical_id != id \
            OR include_reason = 0 \
            OR exclude_reason > 0 \
            {filter_sha}
        )"),
        [])?;
    Ok(n as u64)
}

pub fn list_files_to_stage<R: SqlFileRow>(conn: &Connection, retry_missing_sha: bool)
    -> Result<Vec<R>> {
    let filter_sha = if retry_missing_sha { "" } else { "AND sha1 IS NOT NULL" };
    let mut stmt = conn.prepare(
        &format!("SELECT {} FROM files \
            WHERE phase='sparsified' \
            AND ftype = 'file'\
            AND canonical_id = id \
            AND include_reason > 0 \
            AND exclude_reason = 0 \
            {filter_sha}",
                 R::sql_columns(None)))?;
    let rows = stmt.query_map(
        [],
        |row| R::from_row(row, None))?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}