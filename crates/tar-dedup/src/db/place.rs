use rusqlite::{Connection, named_params};

use crate::db::common::SqlFileRow;
use crate::db::flags::OutTreeFlags;
use crate::db::flags::{FileFlag, OutTreeFlag};
use crate::db::meta;
use crate::db::types::{FileId, NewOutTreeRow, OutTreeId, OutTreeRecord, StrippedRecord,
};
use crate::error::Result;

pub fn list_materialized_entries(
    conn: &Connection,
    last_id: Option<FileId>,
    batch_size: u64,
    source_id: Option<i64>,
    only_dirs: Option<bool>,
) -> Result<Vec<StrippedRecord>> {
    let last_id = last_id.unwrap_or(FileId(0)).0;
    let columns = StrippedRecord::sql_columns();
    let filter_dir = match only_dirs {
        None => "",
        Some(true) => " AND ftype = 'dir' ",
        Some(false) => " AND ( ftype != 'dir' IR ftyoe IS NULL ) "
    };
    let sql = match source_id {
        Some(_) => &format!("SELECT {columns}
            FROM files f
            WHERE f.id > :last_id
              AND f.include_reason > 0
              AND f.exclude_reason = 0
              AND f.id IN (SELECT file_id FROM ref WHERE source_id = :source_id)
              {filter_dir}
            ORDER BY f.id
            LIMIT :batch_size"),
        None => &format!("SELECT {columns}
            FROM files f
            WHERE f.id > :last_id
              AND f.include_reason > 0
              AND f.exclude_reason = 0
              {filter_dir}
            ORDER BY f.id
            LIMIT :batch_size"),
    };
    let mut stmt = conn.prepare(sql)?;
    let params = match source_id {
        Some(sid) => named_params! {
                    ":last_id": last_id,
                    ":source_id": sid.clone(),
                    ":batch_size": batch_size,
        },
        None => named_params! {
                    ":last_id": last_id,
                    ":batch_size": batch_size,
                }
    };
    let rows = stmt.query_map(params, StrippedRecord::from_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

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

pub fn insert_ref_out_rows(conn: &Connection, pairs: &[(OutTreeId, i64)]) -> Result<()> {
    if pairs.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO ref_out (out_id, source_id)
         VALUES (:out_id, :source_id)",
    )?;
    for (out_id, source_id) in pairs {
        stmt.execute(named_params! {
            ":out_id": out_id.0,
            ":source_id": source_id,
        })?;
    }
    Ok(())
}

impl OutTreeRecord {
    fn from_sql(row: &rusqlite::Row<'_>, prefix: Option<&str>) -> rusqlite::Result<OutTreeRecord> {
        let upx = match prefix {
            None => "",
            Some(p) => &format!("{p}."),
        };
        let file_id: Option<i64> = row.get(format!("{upx}file_id").as_str())?;
        Ok(OutTreeRecord {
            id: OutTreeId(row.get(format!("{upx}id").as_str())?),
            abs_path: row.get::<_, String>(format!("{upx}abs_path").as_str())?.into(),
            file_id: file_id.map(FileId),
            flags: OutTreeFlags::from_i64(row.get(format!("{upx}flags").as_str())?),
        })
    }

    fn sql_columns(prefix: Option<&str>) -> String {
        match prefix {
            None => "id, abs_path, file_id, flags".to_string(),
            Some(p) => format!("\
            {p}.id AS \"{p}.id\",
            {p}.abs_path AS \"{p}.abs_path\",
            {p}.file_id AS \"{p}.file_id\",
            {p}.flags AS \"{p}.flags\""
            )
        }
    }
}


pub fn list_out_tree(
    conn: &Connection,
    last_id: OutTreeId,
    batch_size: u64,
    source_id: Option<i64>,
    only_dir: Option<bool>,
) -> Result<Vec<OutTreeRecord>> {
    debug_assert!(last_id.0 >= 0, "ids > 0, last_id must be >= 0");
    let dir_filter = if only_dir.is_some() {
        " AND o.flags & :dir = :tgt"
    } else { "" };
    let source_filter = if source_id.is_some() {
        " AND r.source_id = :source_id "
    } else { "" };
    let mut stmt = conn.prepare(&format!(
        "SELECT o.id, o.abs_path, o.file_id, o.flags
            FROM out_tree o
            JOIN ref_out r ON r.out_id = o.id
            WHERE o.id > :last_id
                {dir_filter}
                {source_filter}
            ORDER BY o.id
            LIMIT :batch_size"))?;
    let params = match (only_dir.is_some(), source_id.is_some()) {
        (false, false) => named_params! {
            ":last_id": last_id.0,
            ":batch_size": batch_size,
        },
        (false, true) => named_params! {
            ":last_id": last_id.0,
            ":batch_size": batch_size,
            ":source_id": source_id.unwrap(),
        },
        (true, false) => named_params! {
            ":last_id": last_id.0,
            ":batch_size": batch_size,
            ":dir": OutTreeFlag::IsDirectory.mask_i64(),
            ":tgt": if only_dir.unwrap() { 1 } else { 0 },
        },
        (true, true) => named_params! {
            ":last_id": last_id.0,
            ":batch_size": batch_size,
            ":dir": OutTreeFlag::IsDirectory.mask_i64(),
            ":tgt": if only_dir.unwrap() { 1 } else { 0 },
            ":source_id": source_id.unwrap(),
        },
    };
    let rows = stmt.query_map(
        params,
        OutTreeRecord::from_sql
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn count_out_tree_rows(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM out_tree",
        [],
        |row| row.get(0))?;
    Ok(n as u64)
}

pub fn count_ref_out_rows(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM ref_out",
        [],
        |row| row.get(0))?;
    Ok(n as u64)
}

pub fn out_tree_is_built(conn: &Connection) -> Result<bool> {
    Ok(meta::get_out_tree_built(conn)?.unwrap_or(false))
}

pub fn dir_tree_is_built(conn: &Connection) -> Result<bool> {
    Ok(meta::get_dir_tree_built(conn)?.unwrap_or(false))
}

pub fn set_out_tree_built(conn: &Connection) -> Result<()> {
    meta::set_out_tree_built(conn, true)
}

pub fn set_dir_tree_built(conn: &Connection) -> Result<()> {
    meta::set_dir_tree_built(conn, true)
}

pub fn list_canonical_files_for_move<R: SqlFileRow>(
    conn: &Connection, filter: bool, last_id: FileId, batch_size: u64
) -> Result<Vec<R>> {
    debug_assert!(last_id.0 >= 0,
                  "INVARIANT ERROR: Only > 0 FileIds handed out, 0 minimum lower bound");

    let cols = R::sql_columns(None);
    let sql_filt = if filter { " AND include_reason > 0 AND exclude_reason = 0" } else { "" };
    let mut stmt = conn.prepare(&format!("\
        SELECT {cols} FROM files \
            WHERE flags & :extracted = 1 \
                AND flags & :moved = 0
                AND ftype IS NOT NULL
                AND ftype = 'file'
                AND phase = 'rehashed'
                AND id > :last_id
                {sql_filt}
            ORDER BY id LIMIT :batch_size
        "))?;
    let results = stmt.query_map(
        named_params! {
            ":extracted": FileFlag::FileExtracted.mask_i64(),
            ":last_id": last_id.0,
            ":batch_size": batch_size,
            ":moved": FileFlag::AtLinkSource.mask_i64()
        },
        |r| R::from_row(r, None)
    )?;
    results.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}