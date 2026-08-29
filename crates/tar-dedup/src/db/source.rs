use rusqlite::{Connection, named_params, OptionalExtension};
use std::path::{Path, PathBuf};

use crate::db::flags::{SourceFlag, SourceFlags};
use crate::db::types::SourceRecord;
use crate::error::Result;

const SOURCE_RECORD_COLUMNS: &str = " id, source, abs_path, original_path, line, flags";

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
    no_recursion: bool,
) -> Result<Option<(i64, PathBuf)>> {
    let path_str = abs_path.to_string_lossy();
    let dir_bit = SourceFlag::IsDirectory.mask_i64();
    let overlap = if no_recursion {
        "rtrim(abs_path, '/') = rtrim(:path, '/')"
    } else {
        "(abs_path = :path
          OR instr(:path, rtrim(abs_path, '/') || '/') = 1
          OR instr(abs_path, rtrim(:path, '/') || '/') = 1)"
    };
    conn.query_row(
        &format!(
            "SELECT id, abs_path FROM source
             WHERE (flags & :dir_bit) != 0
               AND {overlap}
             LIMIT 1"
        ),
        named_params! {
            ":path": path_str.as_ref(),
            ":dir_bit": dir_bit,
        },
        |row| Ok((row.get(0)?, PathBuf::from(row.get::<_, String>(1)?))),
    )
    .optional()
    .map_err(Into::into)
}

type SourceRowMapper = fn(&rusqlite::Row<'_>) -> rusqlite::Result<SourceRecord>;

/// Return up to `batch_size` source rows with `id > starting_id`, ordered by id.
///
/// Pass `starting_id = None` for the first batch, then set it to the last row's
/// `id` from the previous batch until an empty vec is returned.
pub fn list_sources(
    conn: &Connection,
    only_dirs: Option<bool>,
    starting_id: Option<i64>,
    batch_size: u64,
) -> Result<Vec<SourceRecord>> {
    let dir_filter = match only_dirs {
        Some(true) => " AND flags & :DirFlag = 1",
        Some(false) => " AND flags & :DirFlag = 0",
        None => "",
    };
    let after_id = starting_id.unwrap_or(0);
    let dir_flag = SourceFlag::IsDirectory.mask_i64();
    let sql = format!(
        "SELECT {SOURCE_RECORD_COLUMNS} FROM source \
         WHERE id > :starting_id {dir_filter} \
         ORDER BY id ASC \
         LIMIT :batch_size"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = if only_dirs.is_some() {
        stmt.query_map(
            named_params! {
                ":starting_id": after_id,
                ":DirFlag": dir_flag,
                ":batch_size": batch_size,
            },
            from_row as SourceRowMapper,
        )?
    } else {
        stmt.query_map(
            named_params! {
                ":starting_id": after_id,
                ":batch_size": batch_size,
            },
            from_row as SourceRowMapper,
        )?
    };
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceRecord>  {
    Ok(SourceRecord {
        id: row.get::<_, i64>("id")?.into(),
        source: row.get::<_, String>("source")?,
        abs_path: row.get::<_, String>("abs_path")?.into(),
        original_path: row.get::<_, String>("original_path")?.into(),
        line: row.get::<_, i64>("line")?,
        flags: SourceFlags::from_i64(row.get::<_, i64>("flags")?),
    })
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
        assert!(find_overlapping_source(&conn, Path::new("/a/b"), false)
            .unwrap()
            .is_some());
        assert!(find_overlapping_source(&conn, Path::new("/a"), false)
            .unwrap()
            .is_some());
    }

    #[test]
    fn siblings_and_string_prefix_do_not_overlap() {
        let conn = conn();
        insert_dir(&conn, "/a/b");
        assert!(find_overlapping_source(&conn, Path::new("/a/c"), false)
            .unwrap()
            .is_none());
        assert!(find_overlapping_source(&conn, Path::new("/ab"), false)
            .unwrap()
            .is_none());
    }

    #[test]
    fn file_rows_are_ignored() {
        let conn = conn();
        insert_file(&conn, "/a/file.txt", 0);
        insert_file(&conn, "/a/b/c.bin", 1);
        assert!(find_overlapping_source(&conn, Path::new("/a"), false)
            .unwrap()
            .is_none());
        insert_dir(&conn, "/a");
        assert!(find_overlapping_source(&conn, Path::new("/a/b"), false)
            .unwrap()
            .is_some());
    }

    #[test]
    fn no_recursion_same_dir_overlaps_nested_does_not() {
        let conn = conn();
        insert_dir(&conn, "/a/b");
        assert!(find_overlapping_source(&conn, Path::new("/a/b/"), true)
            .unwrap()
            .is_some());
        assert!(find_overlapping_source(&conn, Path::new("/a/b/c"), true)
            .unwrap()
            .is_none());
        assert!(find_overlapping_source(&conn, Path::new("/a"), true)
            .unwrap()
            .is_none());
    }
}
