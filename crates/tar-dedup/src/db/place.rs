use std::path::{Path, PathBuf};

use rusqlite::{named_params, Connection};

use crate::db::common::SqlFileRow;
use crate::db::flags::FileFlag;
use crate::db::types::{FileId, FileType, StrippedRecord};
use crate::error::Result;

/// Leaf row returned when scanning materialized non-directory entries.
#[derive(Debug, Clone)]
pub struct MaterializedLeaf {
    pub id: FileId,
    pub abs_path: PathBuf,
}

pub fn create_prep_ancestor_table(conn: &Connection, relative: bool) -> Result<()> {
    if relative {
        conn.execute_batch(
            "CREATE TEMP TABLE prep_ancestor (
                abs_path  TEXT NOT NULL,
                dir_id    INTEGER,
                source_id INTEGER NOT NULL,
                PRIMARY KEY (abs_path, source_id)
            );",
        )?;
    } else {
        conn.execute_batch(
            "CREATE TEMP TABLE prep_ancestor (
                abs_path TEXT NOT NULL PRIMARY KEY,
                dir_id   INTEGER
            );",
        )?;
    }
    Ok(())
}

pub fn drop_prep_ancestor_table(conn: &Connection) -> Result<()> {
    conn.execute_batch("DROP TABLE IF EXISTS prep_ancestor;")?;
    Ok(())
}

pub fn list_materialized_leaves(
    conn: &Connection,
    last_id: Option<FileId>,
    batch_size: u64,
    source_id: Option<i64>,
) -> Result<Vec<MaterializedLeaf>> {
    let last_id = last_id.unwrap_or(FileId(0)).0;
    let sql = match source_id {
        Some(_) => "SELECT f.id, f.abs_path
            FROM files f
            WHERE f.id > :last_id
              AND f.ftype IS NOT NULL
              AND f.ftype != :dir
              AND f.include_reason > 0
              AND f.exclude_reason = 0
              AND f.id IN (SELECT file_id FROM ref WHERE source_id = :source_id)
            ORDER BY f.id
            LIMIT :batch_size",
        None => "SELECT f.id, f.abs_path
            FROM files f
            WHERE f.id > :last_id
              AND f.include_reason > 0
              AND f.exclude_reason = 0
            ORDER BY f.id
            LIMIT :batch_size",
    };
    let mut stmt = conn.prepare(sql)?;
    match source_id {
        Some(sid) => {
            let rows = stmt.query_map(
                named_params! {
                    ":last_id": last_id,
                    ":dir": FileType::Directory.as_str(),
                    ":source_id": sid,
                    ":batch_size": batch_size,
                },
                map_materialized_leaf,
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        }
        None => {
            let rows = stmt.query_map(
                named_params! {
                    ":last_id": last_id,
                    ":dir": FileType::Directory.as_str(),
                    ":batch_size": batch_size,
                },
                map_materialized_leaf,
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
        }
    }
}

pub fn list_canonical_files_for_move(
    conn: &Connection, filter: bool, last_id: FileId, batch_size: u64
) -> Result<Vec<StrippedRecord>> {
    debug_assert!(last_id.0 >= 0,
                  "INVARIANT ERROR: Only > 0 FileIds handed out, 0 minimum lower bound");

    let cols = StrippedRecord::sql_columns();
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
        StrippedRecord::from_row
    )?;
    results.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}