use crate::db::SqlFileRow;
use crate::db::flags::FileFlag;
use crate::db::types::{FileId, FilterExpression};
use crate::error::Result;
use rusqlite::{Connection, named_params};

const FILTER_ROWS: &str = "id, source, line, expression";

// INFO: During setup a dummy row is added with id 0, so MIN and MAX will return a valid result.
fn add_pattern(
    conn: &Connection, table: &str, from: &str, line: Option<u64>, query: &str, include: bool)
    -> Result<u64> {
    let agg = if include { "MIN(id) - 1" } else { "MAX(id) + 1" };
    let sql = format!(
        "INSERT INTO {table} (id, source, line, expression) VALUES \
         ((SELECT {agg} FROM {table}), :from, :line, :expression)"
    );
    let n = conn.execute(
        &sql,
        named_params! {
            ":from": from,
            ":line": line,
            ":expression": query,
        })?;
    Ok(n as u64)
}

// TODO rename
pub fn add_include_pattern(conn: &Connection, from: &str, line: Option<u64>, query: &str)
    -> Result<u64> {
    add_pattern(conn, "filter_reason_archive", from, line, query, true)
}

pub fn add_exclude_pattern(conn: &Connection, from: &str, line: Option<u64>, query: &str)
    -> Result<u64> {
    add_pattern(conn, "filter_reason_archive", from, line, query, false)
}

pub fn add_include_pattern_extract(conn: &Connection, from: &str, line: Option<u64>, query: &str)
    -> Result<u64> {
    add_pattern(conn, "filter_reason_extract", from, line, query, true)
}

pub fn add_exclude_pattern_extract(conn: &Connection, from: &str, line: Option<u64>, query: &str)
    -> Result<u64> {
    add_pattern(conn, "filter_reason_extract", from, line, query, false)
}

/// Count the different partitions of the filters. The id = 0 dummy row is excluded!
fn count_filters_in(conn: &Connection, table: &str, exclude: Option<bool>) -> Result<u64> {
    let query = match exclude {
        None => format!("SELECT COUNT(*) AS count FROM {table} WHERE id != 0"),
        Some(exclude) => match exclude {
            true => format!("SELECT COUNT(*) AS count FROM {table} WHERE id > 0"),
            false => format!("SELECT COUNT(*) AS count FROM {table} WHERE id < 0"),
        },
    };
    let result: i64 = conn.query_row(&query, [], |row| row.get("count"))?;
    Ok(result as u64)
}

pub fn count_filters(conn: &Connection, exclude: Option<bool>) -> Result<u64> {
    count_filters_in(conn, "filter_reason_archive", exclude)
}

pub fn count_filters_extract(conn: &Connection, exclude: Option<bool>) -> Result<u64> {
    count_filters_in(conn, "filter_reason_extract", exclude)
}

fn get_filters_in(conn: &Connection, table: &str, exclude: bool) -> Result<Vec<FilterExpression>> {
    let filter = if exclude { "id > 0" } else { "id < 0" };
    let query = format!("SELECT {FILTER_ROWS} FROM {table} WHERE {filter}");
    let mut stmt = conn.prepare(&query)?;
    let rows = stmt.query_map([], |row| {
        Ok(FilterExpression {
            id: row.get("id")?,
            from: row.get("source")?,
            line: row.get("line")?,
            expression: row.get("expression")?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn get_filters(conn: &Connection, exclude: bool) -> Result<Vec<FilterExpression>> {
    get_filters_in(conn, "filter_reason_archive", exclude)
}

pub fn get_filters_extract(conn: &Connection, exclude: bool) -> Result<Vec<FilterExpression>> {
    get_filters_in(conn, "filter_reason_extract", exclude)
}

/// In case no filters were given, we promote all files to filters and set the blanket rows
pub fn apply_no_filter(conn: &Connection) -> Result<u64> {
    let n = conn.execute(
        "UPDATE files SET phase = 'filtered', include_reason_archive = -1, \
         exclude_reason_archive = 0",
        [],
    )?;
    Ok(n as u64)
}

/// Promote-all catch-all for the extract filter: the id 0 dummy row is not a usable
/// include, so we hand out a constant negative `-1`. Extract rows keep their phase.
pub fn apply_no_filter_extract(conn: &Connection) -> Result<u64> {
    let n = conn.execute(
        "UPDATE files SET include_reason_extract = -1, exclude_reason_extract = 0",
        [],
    )?;
    Ok(n as u64)
}

// TODO Rename
pub fn get_rows_to_filter<R: SqlFileRow>(
    conn: &Connection, last_id: Option<FileId>, eager_filter: bool, batch_size: u64)
    -> Result<Vec<R>> {
    let last_phase = if eager_filter {
        "'inventoried'"
    } else {
        "'hashed'"
    };
    let last_id_filter = if let Some(_) = last_id {
        " AND id > :last_id "
    } else {
        ""
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM files \
                      WHERE phase = {last_phase} {last_id_filter} \
                      ORDER BY id \
                      LIMIT :batch_size",
        R::sql_columns(None)
    ))?;

    let row_mapper = |r: &rusqlite::Row<'_>| -> rusqlite::Result<R> { R::from_row(r, None) };
    let rows = match last_id {
        None => stmt.query_map(named_params! {":batch_size": batch_size}, row_mapper)?,
        Some(lid) => stmt.query_map(
            named_params! {":batch_size": batch_size, ":last_id": lid.0},
            row_mapper,
        )?,
    };
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

/// Batch over every candidate `files` row (no phase constraint) for the extract filter
/// pass. Extract rows are already archived, so there is no eager/lazy distinction.
pub fn get_rows_to_filter_extract<R: SqlFileRow>(
    conn: &Connection,
    last_id: Option<FileId>,
    batch_size: u64,
) -> Result<Vec<R>> {
    let last_id_filter = if let Some(_) = last_id {
        " AND id > :last_id "
    } else {
        ""
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT {} FROM files \
                      WHERE 1 = 1 {last_id_filter} \
                      ORDER BY id \
                      LIMIT :batch_size",
        R::sql_columns(None)
    ))?;

    let row_mapper = |r: &rusqlite::Row<'_>| -> rusqlite::Result<R> { R::from_row(r, None) };
    let rows = match last_id {
        None => stmt.query_map(named_params! {":batch_size": batch_size}, row_mapper)?,
        Some(lid) => stmt.query_map(
            named_params! {":batch_size": batch_size, ":last_id": lid.0},
            row_mapper,
        )?,
    };
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

// TODO Rename
/// Apply the archive filtering results to the database.
/// The results must have the structure (FileId, include_reason, exclude_reason)!
pub fn apply_filter_result<I: Iterator<Item = (FileId, i64, i64)>>(
    conn: &mut Connection, results: I)
    -> Result<u64> {
    let mut rows_updated = 0u64;
    let transaction = conn.transaction()?;
    let mut stmt = transaction.prepare_cached(
        "UPDATE files \
        SET include_reason_archive = :include_reason, \
            exclude_reason_archive = :exclude_reason, phase = 'filtered' \
        WHERE id = :id",
    )?;

    for (fid, icr, exr) in results {
        rows_updated = rows_updated
            + stmt.execute(named_params! {
                ":id": fid.0,
                ":include_reason": icr,
                ":exclude_reason": exr,
            })? as u64;
    }
    drop(stmt);
    transaction.commit()?;
    Ok(rows_updated)
}

/// Apply the extract filtering results to the database. Phase is left untouched.
/// The results must have the structure (FileId, include_reason, exclude_reason)!
pub fn apply_filter_result_extract<I: Iterator<Item = (FileId, i64, i64)>>(
    conn: &mut Connection,
    results: I,
) -> Result<u64> {
    let mut rows_updated = 0u64;
    let transaction = conn.transaction()?;
    let mut stmt = transaction.prepare_cached(
        "UPDATE files \
        SET include_reason_extract = :include_reason, \
            exclude_reason_extract = :exclude_reason \
        WHERE id = :id",
    )?;

    for (fid, icr, exr) in results {
        rows_updated = rows_updated
            + stmt.execute(named_params! {
                ":id": fid.0,
                ":include_reason": icr,
                ":exclude_reason": exr,
            })? as u64;
    }
    drop(stmt);
    transaction.commit()?;
    Ok(rows_updated)
}

/// Drop every non-dummy extract rule and reset the per-file extract reasons, making the
/// extract filter phase idempotent on resume.
pub fn clear_extract_filters(conn: &mut Connection) -> Result<()> {
    let transaction = conn.transaction()?;
    transaction.execute("DELETE FROM filter_reason_extract WHERE id != 0", [])?;
    transaction.execute(
        "UPDATE files SET include_reason_extract = 0, exclude_reason_extract = 0",
        [],
    )?;
    transaction.commit()?;
    Ok(())
}

/// Function takes care of updating the FileHardlinkCanonical flag if for a given cluster of
/// (dev, inode) the current canonical file is not selected.
/// PRECONDITION:
///   - no_dereference_hardlinks is false.
pub fn fix_up_canonical_flag(conn: &mut Connection) -> Result<(u64, u64)> {
    let transaction = conn.transaction()?;

    let hardlink_mask = FileFlag::FileHardlinkCanonical.mask_i64();

    // Identify (dev, inode) clusters where:
    //  - real cluster size > 1
    //  - the currently-canonical row is excluded
    //  - at least one row is still selected
    // Materialize into a temp table since we need it twice.
    transaction.execute(
        "CREATE TEMP TABLE stale_clusters AS
         SELECT dev, inode FROM files
         WHERE dev IS NOT NULL AND inode IS NOT NULL
         GROUP BY dev, inode
         HAVING COUNT(*) > 1
            AND SUM(CASE WHEN flags & :hardlinks != 0
                          AND NOT (include_reason_archive < 0 AND exclude_reason_archive = 0)
                     THEN 1 ELSE 0 END) = 1
            AND SUM(CASE WHEN include_reason_archive < 0 AND exclude_reason_archive = 0
                     THEN 1 ELSE 0 END) > 0",
        named_params! {":hardlinks": hardlink_mask},
    )?;

    // NOTE: replace `flags & 1` with `flags & :hardlinks` — see below for
    // why raw named_params can't be used inside execute_batch, so we build
    // this with a formatted mask constant instead.
    let downgraded = transaction.execute(
        "UPDATE files
             SET flags = flags & ~:hardlinks
             WHERE flags & :hardlinks != 0
               AND (dev, inode) IN (SELECT dev, inode FROM stale_clusters)",
        named_params! {":hardlinks": hardlink_mask},
    )?;

    let upgraded = transaction.execute(
        "UPDATE files
             SET flags = flags | :hardlinks
             WHERE id IN (
                 SELECT MIN(id) FROM files
                 WHERE include_reason_archive < 0 AND exclude_reason_archive = 0
                   AND (dev, inode) IN (SELECT dev, inode FROM stale_clusters)
                 GROUP BY dev, inode
             )",
        named_params! {":hardlinks": hardlink_mask},
    )?;

    transaction.execute_batch("DROP TABLE stale_clusters")?;
    transaction.commit()?;

    Ok((downgraded as u64, upgraded as u64))
}
