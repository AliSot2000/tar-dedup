use std::path::Path;

use rusqlite::{named_params, Connection};

use crate::error::Result;

/// Insert `(source, path)` if missing, then return the row id.
///
/// Relies on `UNIQUE (source, path)` — no deletes from this table, so a plain
/// insert-or-ignore followed by select is enough.
pub fn add_get_source(conn: &Connection, path: &Path, source: &str, line: Option<u64>) 
    -> Result<i64> {
    let path_str = path.to_string_lossy();
    conn.execute(
        "INSERT OR IGNORE INTO source (source, path, line) VALUES (:source, :path, :line)",
        named_params! {
            ":source": source,
            ":path": path_str.as_ref(),
            ":line": line,
        },
    )?;
    let id = conn.query_row(
        "SELECT id FROM source WHERE source = :source AND path = :path AND line = :line",
        named_params! {
            ":source": source,
            ":path": path_str.as_ref(),
            ":line": line,
        },
        |row| row.get(0),
    )?;
    Ok(id)
}
