use crate::db::common::SqlFileRow;
use crate::db::common::generate_archive_and_extract_filter;
use crate::db::meta;
use crate::db::types::{FileId, NewOutTreeRow, OutTreeId};
use crate::error::{Result, ToPanic};
use rusqlite::{Connection, named_params};

pub fn placement_prologue_done(conn: &Connection) -> Result<bool> {
    Ok(meta::get_placement_prologue_done(conn)?.unwrap_or(false))
}

pub fn set_placement_prologue_done(conn: &Connection) -> Result<()> {
    meta::set_placement_prologue_done(conn, true)
}

pub fn out_tree_is_built(conn: &Connection) -> Result<bool> {
    Ok(meta::get_out_tree_built(conn)?.unwrap_or(false))
}

pub fn set_out_tree_built(conn: &Connection) -> Result<()> {
    meta::set_out_tree_built(conn, true)
}

/// Function iterates through the files table to find all entries which should get materialized
/// based on filtering (include, exclude filter) and on file type. Additionally, a source can be
/// added s.t. only the files which are covered by this source are returned. When
/// `only_valid_new_name` is set, only rows carrying a non-empty `new_name` are returned.
/// When `apply_filters` is false the archive/extract filter gate is dropped (`--overwrite-dir`
/// directory pass must see excluded directories so their metadata can be restored).
pub fn list_materialized_entries<R: SqlFileRow>(
    conn: &Connection,
    last_id: FileId,
    batch_size: u64,
    source_id: Option<i64>,
    only_dirs: Option<bool>,
    only_valid_new_name: bool,
    apply_filters: bool)
    -> Result<Vec<R>> {

    debug_assert!(last_id.0 >= 0, "PRECONDITION FAILED: last_id must always be >= 0");
    let columns = R::sql_columns(Some("f"));
    let filter_dir = match only_dirs {
        None => "",
        Some(true) => " AND f.ftype = 'dir' ",
        // Everything that is not a directory (incl. `unknown`), mirroring the
        // single-pass (`None`) coverage.
        Some(false) => " AND f.ftype != 'dir' ",
    };
    let filter_new_name = if only_valid_new_name {
        " AND f.new_name IS NOT NULL AND f.new_name != '' "
    } else {
        ""
    };
    // `AND`-prefixed gate; empty when filters are bypassed (`--overwrite-dir`
    // directory pass must see excluded directories so their metadata can be
    // restored).
    let filter_include_exclude = if apply_filters {
        format!("AND {}", generate_archive_and_extract_filter(Some("f")))
    } else {
        String::new()
    };

    let (sql, params) = match source_id {
        Some(sid) => {
            (
                format!("
                    SELECT {columns}
                    FROM files f
                    WHERE f.id > :last_id
                        AND f.id IN (SELECT file_id FROM ref WHERE source_id = :source_id)
                        {filter_include_exclude}
                        {filter_dir}
                        {filter_new_name}
                    ORDER BY f.id
                    LIMIT :batch_size"
            ),
                 named_params! {
                     ":last_id": last_id.0,
                     ":source_id": sid.clone(),
                     ":batch_size": batch_size,
            })
        },
        None => {
            (
                format!("
                    SELECT {columns}
                    FROM files f
                    WHERE f.id > :last_id
                        {filter_include_exclude}
                        {filter_dir}
                        {filter_new_name}
                    ORDER BY f.id
                    LIMIT :batch_size"
            ),
                named_params! {
                    ":last_id": last_id.0,
                    ":batch_size": batch_size,
            })
        },
    };
    let mut stmt = conn.prepare(&sql).to_panic()?;
    let rows = stmt.query_map(
        params,
        |r| R::from_row(r, Some("f"))).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// Record the member-relative `--strip-components` result on a file row.
/// `None` clears the column (no rename applies); `Some("")` marks the empty-name
/// skip sentinel consumed by the out_tree build.
pub fn set_file_new_name(conn: &Connection, file_id: FileId, new_name: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE files SET new_name = :new_name WHERE id = :id",
        named_params! {
            ":new_name": new_name,
            ":id" : file_id.0
        },
    ).to_panic()?;
    Ok(())
}

/// Insert a new OutTree row into the table. Function then returns the ids of all rows inserted
/// based on the abs_path
pub fn insert_out_tree_rows(conn: &Connection, rows: &[NewOutTreeRow]) -> Result<Vec<OutTreeId>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut insert = conn.prepare("
        INSERT OR IGNORE INTO out_tree (abs_path, file_id, flags)
        VALUES (:abs_path, :file_id, :flags)").to_panic()?;

    for row in rows {
        insert.execute(named_params! {
            ":abs_path": row.abs_path.to_string_lossy().as_ref(),
            ":file_id": row.file_id.map(|id| id.0),
            ":flags": row.flags.to_i64(),
        }).to_panic()?;
    }
    let mut ids = Vec::with_capacity(rows.len());
    // get the newly inserted ids.
    for row in rows {
        let id: i64 = conn.query_row(
            "SELECT id FROM out_tree WHERE abs_path = :abs_path",
            named_params! { ":abs_path": row.abs_path.to_string_lossy().as_ref() },
            |r| r.get(0),
        ).to_panic()?;
        ids.push(OutTreeId(id));
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::types::StrippedRecord;

    /// Minimal `files` surface for `list_materialized_entries`: the StrippedRecord
    /// columns plus the four filter-reason columns the gate reads.
    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("
            CREATE TABLE files (
                id INTEGER PRIMARY KEY,
                abs_path TEXT NOT NULL,
                ext TEXT NOT NULL DEFAULT '',
                size INTEGER NOT NULL DEFAULT 0,
                sha1 BLOB,
                mtime INTEGER, atime INTEGER, ctime INTEGER,
                ftype TEXT NOT NULL,
                canonical_id INTEGER,
                flags INTEGER NOT NULL DEFAULT 0,
                phase TEXT NOT NULL DEFAULT 'inventoried',
                inode INTEGER,
                dev INTEGER,
                new_name TEXT,
                include_reason_archive INTEGER NOT NULL DEFAULT 0,
                exclude_reason_archive INTEGER NOT NULL DEFAULT 0,
                include_reason_extract INTEGER NOT NULL DEFAULT 0,
                exclude_reason_extract INTEGER NOT NULL DEFAULT 0
            );
        ",)
            .unwrap();
        conn
    }

    /// Insert a row; include reasons are `-1` (included) so the archive+extract
    /// gate passes when `apply_filters` is set.
    fn insert_row(conn: &Connection, id: i64, abs_path: &str, ftype: &str) {
        conn.execute(
            "INSERT INTO files \
             (id, abs_path, ftype, ext, include_reason_archive, include_reason_extract) \
             VALUES (:id, :abs_path, :ftype, '', -1, -1)",
            named_params! {
                ":id": id,
                ":abs_path": abs_path,
                ":ftype": ftype,
            },
        ).unwrap();
    }

    fn exclude_extract(conn: &Connection, id: i64) {
        conn.execute(
            "UPDATE files SET exclude_reason_extract = 3 WHERE id = :id",
            named_params! { ":id": id },
        ).unwrap();
    }

    fn ids(conn: &Connection, only_dirs: Option<bool>, apply_filters: bool) -> Vec<i64> {
        let rows = list_materialized_entries::<StrippedRecord>(
            conn, FileId(0), 100, None, only_dirs, false, apply_filters
        )
        .unwrap();
        rows.iter().map(|r| r.id.0).collect::<Vec<_>>()
    }

    #[test]
    fn dirs_pass_bypasses_filters_for_dirs() {
        let conn = conn();
        insert_row(&conn, 1, "/a", "dir");
        insert_row(&conn, 2, "/b", "dir");
        exclude_extract(&conn, 2);
        // --overwrite-dir: excluded dir still materialized.
        let dirs = ids(&conn, Some(true), false);
        assert_eq!(dirs, vec![1i64, 2]);
    }

    #[test]
    fn dirs_pass_is_filter_aware_without_flag() {
        let conn = conn();
        insert_row(&conn, 1, "/a", "dir");
        insert_row(&conn, 2, "/b", "dir");
        exclude_extract(&conn, 2);
        let dirs = ids(&conn, Some(true), true);
        assert_eq!(dirs, vec![1i64]);
    }

    #[test]
    fn rest_pass_still_filters() {
        let conn = conn();
        insert_row(&conn, 1, "/a", "file");
        insert_row(&conn, 2, "/b", "file");
        exclude_extract(&conn, 2);
        let rest = ids(&conn, Some(false), true);
        assert_eq!(rest, vec![1i64]);
    }

    #[test]
    fn unknown_rows_land_in_rest_pass() {
        let conn = conn();
        insert_row(&conn, 1, "/a", "dir");
        insert_row(&conn, 2, "/u", "unknown");
        insert_row(&conn, 3, "/f", "file");
        let rest = ids(&conn, Some(false), true);
        assert_eq!(rest, vec![2i64, 3]);
    }
}
