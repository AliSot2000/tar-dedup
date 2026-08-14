use rusqlite::{Connection, named_params};
use crate::db::types::FilterExpression;
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