use crate::db::common::SqlFileRow;
use crate::db::common::generate_archive_and_extract_filter;
use crate::db::meta;
use crate::db::types::{FileId, NewOutTreeRow, OutTreeId};
use crate::error::Result;
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
pub fn list_materialized_entries<R: SqlFileRow>(
    conn: &Connection,
    last_id: FileId,
    batch_size: u64,
    source_id: Option<i64>,
    only_dirs: Option<bool>,
    only_valid_new_name: bool)
    -> Result<Vec<R>> {

    debug_assert!(last_id.0 >= 0, "PRECONDITION FAILED: last_id must always be >= 0");
    let columns = R::sql_columns(Some("f"));
    let filter_dir = match only_dirs {
        None => "",
        Some(true) => " AND f.ftype = 'dir' ",
        Some(false) => " AND f.ftype NOT IN ('dir', 'unknown') ", // INFO: ftype IS NOT NULL!
    };
    let filter_new_name = if only_valid_new_name {
        " AND f.new_name IS NOT NULL AND f.new_name != '' "
    } else {
        ""
    };
    let sql = match source_id {
        Some(_) => &format!(
            "SELECT {columns}
            FROM files f
            WHERE f.id > :last_id
              AND {}
              AND f.id IN (SELECT file_id FROM ref WHERE source_id = :source_id)
              {filter_dir}
              {filter_new_name}
            ORDER BY f.id
            LIMIT :batch_size",
            generate_archive_and_extract_filter(Some("f"))
        ),
        None => &format!(
            "SELECT {columns}
            FROM files f
            WHERE f.id > :last_id
              AND {}
              {filter_dir}
              {filter_new_name}
            ORDER BY f.id
            LIMIT :batch_size",
            generate_archive_and_extract_filter(Some("f"))
        ),
    };
    let mut stmt = conn.prepare(sql)?;
    let params = match source_id {
        Some(sid) => named_params! {
                    ":last_id": last_id.0,
                    ":source_id": sid.clone(),
                    ":batch_size": batch_size,
        },
        None => named_params! {
            ":last_id": last_id.0,
            ":batch_size": batch_size,
        },
    };
    let rows = stmt.query_map(params, |r| R::from_row(r, None))?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
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
    )?;
    Ok(())
}

/// Insert a new OutTree row into the table. Function then returns the ids of all rows inserted
/// based on the abs_path
pub fn insert_out_tree_rows(conn: &Connection, rows: &[NewOutTreeRow]) -> Result<Vec<OutTreeId>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let mut insert = conn.prepare(
        "INSERT OR IGNORE INTO out_tree (abs_path, file_id, flags)
         VALUES (:abs_path, :file_id, :flags)",
    )?;
    for row in rows {
        insert.execute(named_params! {
            ":abs_path": row.abs_path.to_string_lossy().as_ref(),
            ":file_id": row.file_id.map(|id| id.0),
            ":flags": row.flags.to_i64(),
        })?;
    }
    let mut ids = Vec::with_capacity(rows.len());
    for row in rows {
        let id: i64 = conn.query_row(
            "SELECT id FROM out_tree WHERE abs_path = :abs_path",
            named_params! { ":abs_path": row.abs_path.to_string_lossy().as_ref() },
            |r| r.get(0),
        )?;
        ids.push(OutTreeId(id));
    }
    Ok(ids)
}
