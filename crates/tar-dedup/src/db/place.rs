use rusqlite::{named_params, Connection};

use crate::db::types::{FileId, FileType, StrippedRecord};
use crate::db::common::SqlFileRow;
use crate::error::Result;


/// Get a vector of up to batch_size structs. Start selecting from last_id.
/// Only directory entries are returned.
pub fn list_directories(conn: &Connection, last_id: Option<FileId>, batch_size: u64)
    -> Result<Vec<StrippedRecord>> {
    let rows = StrippedRecord::sql_columns();
    let last_id = last_id.unwrap_or(FileId(0)).0;
    let mut stmt = conn.prepare(&format!(
        "SELECT {rows} FROM files WHERE id > :last_id AND ftype = :dir \
        ORDER BY id ASC LIMIT :batch_size "))?;
    let results = stmt.query_map(
        named_params! {
            ":last_id": last_id,
            ":batch_size": batch_size,
            ":dir": FileType::Directory.as_str(),
        },
        StrippedRecord::from_row
    )?;
    results.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Select the subset of the abs_path tree which is walkable from a given source id. 
/// A vector of up to batch_size of structs is returned. Start selection from last_file_id, only 
/// directory entries are returned.
pub fn list_directories_from_source(
    conn: &Connection, source_id: i64, last_id: Option<FileId>, batch_size: u64) 
    -> Result<Vec<StrippedRecord>> {
    let rows = StrippedRecord::sql_columns();
    let last_id = last_id.unwrap_or(FileId(0)).0;
    let mut stmt = conn.prepare(&format!(
        "SELECT {rows} FROM files JOIN ref ON files.id = ref.file_id \
        WHERE id > :last_id AND ftype = :dir AND ref.source_id = :source_id \
        ORDER BY id ASC LIMIT :batch_size "))?;
    let results = stmt.query_map(
        named_params! {
            ":last_id": last_id,
            ":batch_size": batch_size,
            ":dir": FileType::Directory.as_str(),
            ":source_id": source_id,
        },
        StrippedRecord::from_row
    )?;
    results.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}