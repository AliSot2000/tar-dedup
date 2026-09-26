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
