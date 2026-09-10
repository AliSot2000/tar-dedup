//! Query layer for the permissions (metadata restore) phase.
//!
//! Rows are returned deepest-first (delay-restore / bottom-up order), so directory
//! metadata is applied after the contents beneath it. Only `Placed` canonical rows
//! (`o.canonical_id = o.id`) are considered: hardlink duplicates share the inode and
//! hence already carry the metadata once the canonical row is handled.

use rusqlite::{Connection, named_params};

use crate::db::common::SqlFileRow;
use crate::db::flags::{FileFlag, OutTreeFlag};
use crate::db::types::OutTreeRecord;
use crate::error::Result;

/// Depth of an `out_tree` row in the extraction tree (number of path components).
/// Ordered deepest-first so directory metadata lands after its contents.
fn depth_order_clause(alias: &str) -> String {
    format!(
        "(LENGTH({a}.abs_path) - LENGTH(REPLACE({a}.abs_path, '/', ''))) DESC, {a}.id",
        a = alias,
    )
}

/// Rows whose metadata must still be applied, deepest first.
///
/// Filters to canonical (`canonical_id = id`) `Placed` rows that are neither
/// already-applied (`PermissionsApplied`) nor errored (`ErrorWhileApplyingMetadata`).
pub fn list_out_tree_for_permissions_non_dir<R: SqlFileRow>(
    conn: &Connection,
    batch_size: u64,
) -> Result<Vec<(R, OutTreeRecord)>> {
    let file_cols = R::sql_columns(Some("f"));
    let out_cols = OutTreeRecord::sql_columns(Some("o"));
    let order = depth_order_clause("o");
    let mut stmt = conn.prepare(&format!(
        "SELECT {file_cols}, {out_cols}
         FROM out_tree o
         JOIN files f ON f.id = o.file_id
         WHERE o.flags & :placed != 0
           AND o.flags & :applied = 0
           AND o.flags & :error = 0
           AND o.flags & :is_dir = 0
           AND o.canonical_id = o.id
         ORDER BY {order}
         LIMIT :batch_size"
    ))?;
    let rows = stmt.query_map(
        named_params! {
            ":batch_size": batch_size,
            ":placed": OutTreeFlag::Placed.mask_i64(),
            ":applied": OutTreeFlag::PermissionsApplied.mask_i64(),
            ":error": OutTreeFlag::ErrorWhileApplyingMetadata.mask_i64(),
            ":is_dir": OutTreeFlag::IsDirectory.mask_i64(),
        },
        |row| {
            let r = R::from_row(row, Some("f"))?;
            let o = OutTreeRecord::from_sql(row, Some("o"))?;
            Ok((r, o))
        },
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Directory rows whose metadata must still be applied, deepest first.
///
/// Only applied when `--overwrite-dir` is set. Directories are never marked `Placed`
/// or errored: they are created up-front by `prepare_extraction_dir` (aborting the
/// whole run on failure). Ancestor rows injected by `ensure_parent` have a `NULL`
/// `file_id` (no catalog row, so no metadata to apply) — hence the LEFT JOIN.
pub fn list_out_tree_for_permissions_dirs<R: SqlFileRow>(
    conn: &Connection,
    batch_size: u64,
) -> Result<Vec<(Option<R>, OutTreeRecord)>> {
    let file_cols = R::sql_columns(Some("f"));
    let out_cols = OutTreeRecord::sql_columns(Some("o"));
    let order = depth_order_clause("o");
    let mut stmt = conn.prepare(&format!(
        "SELECT {file_cols}, {out_cols}
         FROM out_tree o
         LEFT JOIN files f ON f.id = o.file_id
         WHERE o.flags & :is_dir != 0
           AND o.flags & :applied = 0
         ORDER BY {order}
         LIMIT :batch_size"
    ))?;
    let rows = stmt.query_map(
        named_params! {
            ":batch_size": batch_size,
            ":applied": OutTreeFlag::PermissionsApplied.mask_i64(),
            ":is_dir": OutTreeFlag::IsDirectory.mask_i64(),
        },
        |row| {
            let r: Option<R> = match row.get::<_, Option<i64>>("f.id")? {
                None => None,
                Some(_) => Some(R::from_row(row, Some("f"))?),
            };
            let o = OutTreeRecord::from_sql(row, Some("o"))?;
            Ok((r, o))
        },
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Count of non-dir rows still needing metadata (for progress reporting).
pub fn count_out_tree_for_permissions_non_dir(conn: &Connection) -> Result<u64> {
    count_out_tree_for_permissions(conn, false)
}

/// Count of directory rows still needing metadata (only relevant with `--overwrite-dir`).
pub fn count_out_tree_for_permissions_dirs(conn: &Connection) -> Result<u64> {
    count_out_tree_for_permissions(conn, true)
}

fn count_out_tree_for_permissions(conn: &Connection, dirs: bool) -> Result<u64> {
    // Directories are never marked Placed/errored (created up-front by
    // prepare_extraction_dir); NULL file_id ancestors (ensure_parent) are included.
    let (from, where_extra, params) = if dirs {
        (
            "FROM out_tree o",
            "AND o.flags & :is_dir != 0",
            named_params! {
                ":applied": OutTreeFlag::PermissionsApplied.mask_i64(),
                ":is_dir": OutTreeFlag::IsDirectory.mask_i64(),
            }
        )
    } else {
        (
            "FROM out_tree o",
            "AND o.flags & :is_dir = 0 AND o.canonical_id = o.id",
            named_params! {
                ":placed": OutTreeFlag::Placed.mask_i64(),
                ":applied": OutTreeFlag::PermissionsApplied.mask_i64(),
                ":error": OutTreeFlag::ErrorWhileApplyingMetadata.mask_i64(),
                ":is_dir": OutTreeFlag::IsDirectory.mask_i64(),
            }
        )
    };
    let sql = format!(
        "SELECT COUNT(*)
         {from}
         WHERE o.flags & :applied = 0
           {where_extra}"
    );
    let n: i64 = conn.query_row(&sql, params, |row| row.get(0))?;
    Ok(n as u64)
}

/// (metadata applied, metadata with error)
pub fn apply_flags_to_files(conn: &Connection) -> Result<(u64, u64)> {
    // Placed if all are placed
    let applied = conn.execute(
        "UPDATE files SET flags = flags | :file_placed
            WHERE files.id IN (SELECT file_id FROM out_tree)
                AND files.id NOT IN (SELECT file_id FROM out_tree WHERE flags & :out_placed = 0)",
        named_params! {
            ":file_placed": FileFlag::AppliedMetadata.mask_i64(),
            ":out_placed": OutTreeFlag::PermissionsApplied.mask_i64(),
        },
    )?;
    // Errored if any are errored
    let errored = conn.execute(
        "UPDATE files SET flags = flags | :file_error
            WHERE files.id IN (SELECT file_id FROM out_tree WHERE flags & :out_error = 0)",
        named_params! {
        ":file_reflink": FileFlag::ErrorWhileApplyingMetadata.mask_i64(),
        ":out_reflink": OutTreeFlag::ErrorWhileApplyingMetadata.mask_i64()
        },
    )?;
    Ok((applied as u64, errored as u64))
}
