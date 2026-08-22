use rusqlite::{Connection, named_params, OptionalExtension};
use std::path::{Path, PathBuf};

use crate::db::flags::{SourceFlag, SourceFlags};
use crate::error::Result;

/// Insert `(source, path)` if missing, then return the row id.
///
/// Relies on `UNIQUE (source, path)` — no deletes from this table, so a plain
/// insert-or-ignore followed by select is enough.
pub fn add_get_source(conn: &Connection,
                      abs_path: &Path,
                      source: &str,
                      line: Option<u64>,
                      original_path: Option<&Path>,
                      flags: SourceFlags)
                      -> Result<i64> {
    let path_str = abs_path.to_string_lossy();
    conn.execute(
        "INSERT OR IGNORE INTO source (source, abs_path, line, original_path, flags) \
        VALUES (:source, :abs_path, :line, :org_path, :flags)",
        named_params! {
            ":source": source,
            ":abs_path": path_str.as_ref(),
            ":line": line,
            ":org_path": original_path.map(|p| p.to_string_lossy()),
            ":flags": flags.to_i64(),
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

/// First **directory** source whose `abs_path` overlaps `abs_path`.
///
/// Component boundary is `INSTR(haystack, prefix || '/') = 1` (and equality).
/// `rtrim(..., '/')` so `/` is a prefix of every absolute path (`'' || '/'` → `/`).
/// Only [`SourceFlag::IsDirectory`] rows are considered so a 2M-line `--files-from`
/// of files is not scanned when a later directory line is checked.
pub fn find_overlapping_source(
    conn: &Connection,
    abs_path: &Path,
) -> Result<Option<(i64, PathBuf)>> {
    let path_str = abs_path.to_string_lossy();
    let dir_bit = SourceFlag::IsDirectory.mask_i64();
    conn.query_row(
        "SELECT id, abs_path FROM source
         WHERE (flags & :dir_bit) != 0
           AND (
                abs_path = :path
             OR instr(:path, rtrim(abs_path, '/') || '/') = 1
             OR instr(abs_path, rtrim(:path, '/') || '/') = 1
           )
         LIMIT 1",
        named_params! {
            ":path": path_str.as_ref(),
            ":dir_bit": dir_bit,
        },
        |row| Ok((row.get(0)?, PathBuf::from(row.get::<_, String>(1)?))),
    )
    .optional()
    .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::path::Path;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE source (
                id INTEGER PRIMARY KEY,
                source TEXT NOT NULL,
                abs_path TEXT NOT NULL,
                original_path TEXT,
                line INTEGER,
                flags INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX idx_source_dirs ON source(abs_path)
            WHERE (flags & 1) != 0;",
        )
        .unwrap();
        conn
    }

    fn insert_dir(conn: &Connection, path: &str) {
        add_get_source(
            conn,
            Path::new(path),
            "--input-dir",
            Some(0),
            None,
            SourceFlags::default().with(SourceFlag::IsDirectory, true),
        )
        .unwrap();
    }

    fn insert_file(conn: &Connection, path: &str, line: u64) {
        add_get_source(
            conn,
            Path::new(path),
            "--files-from=x",
            Some(line),
            None,
            SourceFlags::default(),
        )
        .unwrap();
    }

    #[test]
    fn overlap_nested_and_identical() {
        let conn = conn();
        insert_dir(&conn, "/a");
        assert!(find_overlapping_source(&conn, Path::new("/a/b"))
            .unwrap()
            .is_some());
        assert!(find_overlapping_source(&conn, Path::new("/a"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn siblings_and_string_prefix_do_not_overlap() {
        let conn = conn();
        insert_dir(&conn, "/a/b");
        assert!(find_overlapping_source(&conn, Path::new("/a/c"))
            .unwrap()
            .is_none());
        assert!(find_overlapping_source(&conn, Path::new("/ab"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn file_rows_are_ignored() {
        let conn = conn();
        insert_file(&conn, "/a/file.txt", 0);
        insert_file(&conn, "/a/b/c.bin", 1);
        assert!(find_overlapping_source(&conn, Path::new("/a"))
            .unwrap()
            .is_none());
        insert_dir(&conn, "/a");
        assert!(find_overlapping_source(&conn, Path::new("/a/b"))
            .unwrap()
            .is_some());
    }
}
