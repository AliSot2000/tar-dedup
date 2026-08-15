use rusqlite::{Connection, named_params};
use crate::db::flags::FileFlag;
use crate::db::SqlFileRow;
use crate::db::types::{FileId, FilterExpression};
use crate::error::Result;

const FILTER_ROWS: &str = "id, source, line, expression";

/// Stub until a real filter stage exists: advance all hashed rows to filtered.
pub fn promote_hashed_to_filtered(conn: &Connection) -> Result<u64> {
    let n = conn.execute(
        "UPDATE files SET phase = 'filtered' WHERE phase = 'hashed'",
        [],
    )?;
    Ok(n as u64)
}

// INFO: During setup a dummy row is added with id 0, so MIN and MAX will return a valid result.
pub fn add_include_pattern(
    conn: &Connection, from: &str, line: Option<u64>, query: &str)
    -> Result<u64> {

    let n = conn.execute("INSERT INTO filter_reason (id, source, line, expression) \
    VALUES (\
        (SELECT MIN(id) FROM filter_reason), \
        :from, \
        :line, \
        :expression" ,
    named_params!{
        ":from": from,
        ":line": line,
        ":expression": query,
    })?;
    Ok(n as u64)
}

pub fn add_exclude_pattern(
    conn: &Connection, from: &str, line: Option<u64>, query: &str)
    -> Result<u64> {
    let n = conn.execute("INSERT INTO filter_reason (id, source, line, expression) \
    VALUES (\
        (SELECT MAX(id) FROM filter_reason), \
        :from, \
        :line, \
        :expression" ,
    named_params!{
        ":from": from,
        ":line": line,
        ":expression": query,
    })?;
    Ok(n as u64)
}

/// Count the different partitions of the filters. The id = 0 dummy row is excluded!
pub fn count_filters(conn: &Connection, exclude: Option<bool>) -> Result<u64> {
    let query = match exclude {
        None => "SELECT COUNT(*) AS count FROM filter_reason WHERE id != 0",
        Some(exclude ) => match exclude {
            true => "SELECT COUNT(*) AS count FROM filter_reason WHERE id > 0",
            false => "SELECT COUNT(*) AS count FROM filter_reason WHERE id < 0",
        }
    };
    let result: i64 = conn.query_row(query, [], |row| row.get("count"))?;
    Ok(result as u64)
}

pub fn get_filters(conn: &Connection, exclude: bool) -> Result<Vec<FilterExpression>> {
    let filter = if exclude { "id > 0" } else { "id < 0" };
    let query = format!("SELECT {FILTER_ROWS} FROM filter_reason WHERE {filter}");
    let mut stmt = conn.prepare(&query)?;
    let rows = stmt.query_map([], |row| {
        Ok(FilterExpression {
            id: row.get("id")?,
            from: row.get("source")?,
            line: row.get("line")?,
            expression: row.get("expression")?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)}

/// In case no filters were given, we promote all files to filters and set the blanket rows
pub fn apply_no_filter(conn: &Connection) -> Result<u64> {
    let n = conn.execute(
        "UPDATE files SET phase = 'filtered', include_reason = 1, exclude_reason = 0",
        [],
    )?;
    Ok(n as u64)
}

pub fn get_rows_to_filter<R: SqlFileRow>(
    conn: &Connection, last_id: Option<FileId>, eager_filter: bool, batch_size: u64)
    -> Result<Vec<R>> {
    let last_phase = if eager_filter { "'inventoried'" } else { "'hashed'" };
    let last_id_filter = if let Some(_) = last_id { " AND id > :last_id " } else { "" };
    let mut stmt = conn.prepare(
        &format!("SELECT * FROM files \
                      WHERE phase = {last_phase} {last_id_filter} \
                      ORDER BY id \
                      LIMIT :batch_size"))?;

    let rows = match last_id {
        None => {
            stmt.query_map(named_params! {":batch_size": batch_size}, R::from_row)?

        }
        Some(lid) => {
            stmt.query_map(
                named_params! {":batch_size": batch_size, ":last_id": lid.0}, R::from_row)?
        }
    };
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Function applies the results of the filtering of the files to the database.
/// The results must have the structure (FileId, include_reason, exclude_reason)!
pub fn apply_filter_result<I: Iterator<Item = (FileId, i64, i64)>>(conn: &mut Connection, results: I)
    -> Result<u64> {
    let mut rows_updated = 0u64;
    let transaction = conn.transaction()?;
    let mut stmt = transaction.prepare_cached(
        "UPDATE files \
        SET include_reason = :include_reason, exclude_reason: exclude_reason, phase = 'filtered' \
        WHERE id = :id")?;

    for (fid, icr, exr) in results {
        rows_updated = rows_updated + stmt.execute(named_params! {
            ":file_id": fid.0,
            ":include_reason": icr,
            ":exclude_reason": exr,
        })? as u64;
    }
    drop(stmt);
    transaction.commit()?;
    Ok(rows_updated)
}


/// Function takes care of updating the FileHardlinkCanonical flag if for a given cluster of
/// (dev, inode) the current canonical file is not selected.
/// PRECONDITION:
///   - no_dereference_hardlinks is false.
pub fn fix_up_canonical_flag(conn: &mut Connection) -> Result<u64> {
    let transaction = conn.transaction()?;
    let downgraded = transaction.execute(
        "UPDATE files \
             SET flags & ~:hardlinks \
             WHERE flags & :hardlinks = 1 \
                AND (dev, inode) IN (SELECT dev, inode \
                                     FROM files \
                                     WHERE dev IS NOT NULL AND inode IS NOT NULL \
                                     GROUP BY (dev, inode) \
                                     HAVING COUNT(*) > 1",
        named_params! {":hardlinks": FileFlag::FileHardlinkCanonical.mask_i64()})?;
    let upgrade = transaction.execute(
        "UPDATE files \
             SET flags | ~:hardlinks\
             WHERE id IN (SELECT MIN(id) \
                          ROM files \
                          WHERE AND include_reason > 0 AND exclude_reason = 0 AND",
                                      named_params! {":hardlinks": FileFlag::FileHardlinkCanonical.mask_i64()})?;
    transaction.commit()?;

    Ok(0)
}