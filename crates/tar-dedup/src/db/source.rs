use rusqlite::{Connection, named_params};
use std::path::Path;

use crate::error::Result;

/// Insert `(source, path)` if missing, then return the row id.
///
/// Relies on `UNIQUE (source, path)` — no deletes from this table, so a plain
/// insert-or-ignore followed by select is enough.
pub fn add_get_source(conn: &Connection,
                      abs_path: &Path,
                      source: &str,
                      line: Option<u64>,
                      original_path: Option<&Path>)
                      -> Result<i64> {
    let path_str = abs_path.to_string_lossy();
    conn.execute(
        "INSERT OR IGNORE INTO source (source, abs_path, line, original_path) \
        VALUES (:source, :abs_path, :line, :org_path)",
        named_params! {
            ":source": source,
            ":abs_path": path_str.as_ref(),
            ":line": line,
            ":org_path": original_path.map(|p| p.to_string_lossy()),
        },
    )?;
    let id = conn.query_row(
        "SELECT id FROM source WHERE source = :source AND abs_path = :path AND line = :line",
        named_params! {
            ":source": source,
            ":path": path_str.as_ref(),
            ":line": line,
        },
        |row| row.get(0),
    )?;
    Ok(id)
}
