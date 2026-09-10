//! File contains db functions to check the integrity of the database

use rusqlite::{Connection, named_params};

use crate::db::common::SqlFileRow;
use crate::db::types::FileId;
use crate::error::Result;

/// For hardlink detection, grouping based on dev, inode is required. Count all files which could
/// not be treated because dev or inode is missing.
pub fn count_missing_dev_inode(conn: &Connection) -> Result<u64> {
    let res: i64 = conn.query_row(
        "SELECT COUNT(*) AS count \
        FROM files WHERE ftype = 'file' AND (inode IS NULL OR dev IS NULL)",
        [],
        |r| r.get("count"))?;
    Ok(res as u64)
}

/// For hardlink detection, grouping based on dev, inode is required. List all files which could
/// not be treated because dev or inode is missing.
pub fn list_missing_dev_inode<R: SqlFileRow>(conn: &Connection, last_id: &FileId, batch_size: u64)
    -> Result<Vec<R>> {
    let cols = R::sql_columns(None);
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols} \
         FROM files \
         WHERE ftype = 'file' \
         AND id > :last_id
         AND (inode IS NULL \
              OR dev IS NULL)\
         ORDER BY id
         LIMIT :batch_size"))?;
    let rows = stmt.query_map(named_params! {
        ":last_id": last_id.0,
        ":batch_size": batch_size
    }, |row| R::from_row(row, None)
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn count_double_canonical_dev_inode_group(conn: &Connection) -> Result<u64> {
    let res: i64 = conn.query_row(
        "SELECT COUNT(*) AS count
        FROM (
            SELECT f.dev, f.inode
            FROM files AS f
            WHERE f.ftype = 'file'
                AND f.canonical_id IS NOT NULL
                AND f.dev IS NOT NULL
                AND f.inode IS NOT NULL
            GROUP BY f.dev, f.inode
            HAVING COUNT(DISTINCT f.canonical_id) > 1)",
        [],
        |r| r.get("count"))?;
    Ok(res as u64)
}

pub fn list_double_canonical_dev_inode_group<R: SqlFileRow>(
    conn: &Connection, last_id: &FileId, batch_size: u64)
    -> Result<Vec<R>> {
    let cols = R::sql_columns(None);
    let mut stmt = conn.prepare(&format!("
        SELECT {cols}
        FROM files
        WHERE ftype = 'file'
            AND id > :last_id
            AND (dev, inode) IN (SELECT f.dev, f.inode
                FROM files AS f
                WHERE f.ftype = 'file'
                    AND f.canonical_id IS NOT NULL
                    AND f.dev IS NOT NULL
                    AND f.inode IS NOT NULL
                GROUP BY f.dev, f.inode
                HAVING COUNT(DISTINCT f.canonical_id) > 1))
        ORDER BY id
        LIMIT :batch_size
    "))?;
    let rows = stmt.query_map(
        named_params! {
            "last_id:": last_id.0,
            ":batch_size": batch_size,
        },
        |row| R::from_row(row, None)
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Check how many rows fail the implication name -> id
pub fn count_id_implication(conn: &Connection) -> Result<u64> {
    let res: i64 = conn.query_row(
        "SELECT COUNT(*) AS count
        FROM files
        WHERE (uid IS NULL AND username IS NOT NULL)
            OR (gid IS NULL AND groupname IS NOT NULL)",
        [],
        |r| r.get("count"))?;
    Ok(res as u64)
}

/// Check how many rows fail the implication name -> id
pub fn count_missing_unix_infos(conn: &Connection) -> Result<u64> {
    let res: i64 = conn.query_row(
        "SELECT COUNT(*) AS count
        FROM files
        WHERE uid IS NULL
            OR gid IS NULL
            OR mode IS NULL
            OR inode IS NULL
            OR dev IS NULL",
        [],
        |r| r.get("count"))?;
    Ok(res as u64)
}

// TODO integrity check that only filter allow files are extracted.
