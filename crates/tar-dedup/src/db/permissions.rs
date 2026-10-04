//! Query layer for the permissions (metadata restore) phase.
//!
//! Rows are returned deepest-first (delay-restore / bottom-up order), so directory
//! metadata is applied after the contents beneath it. Only `Placed` canonical rows
//! (`o.canonical_id = o.id`) are considered: hardlink duplicates share the inode and
//! hence already carry the metadata once the canonical row is handled.

use rusqlite::{Connection, named_params};

use crate::db::common::{SqlFileRow, with_transaction};
use crate::db::flags::{FileFlag, OutTreeFlag, set_file_flag, set_out_tree_flag};
use crate::db::types::{FileId, OutTreeId, OutTreeRecord};
use crate::error::{Result, ToPanic};

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
    )).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":batch_size": batch_size,
            ":placed": OutTreeFlag::Placed.mask_i64(),
            ":applied": OutTreeFlag::AppliedMetadata.mask_i64(),
            ":error": OutTreeFlag::ErrorWhileApplyingMetadata.mask_i64(),
            ":is_dir": OutTreeFlag::IsDirectory.mask_i64(),
        },
        |row| {
            let r = R::from_row(row, Some("f"))?;
            let o = OutTreeRecord::from_sql(row, Some("o"))?;
            Ok((r, o))
        },
    ).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
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
    )).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":batch_size": batch_size,
            ":applied": OutTreeFlag::AppliedMetadata.mask_i64(),
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
    ).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// Count of non-dir rows still needing metadata (for progress reporting).
pub fn count_out_tree_for_permissions_non_dir(conn: &Connection) -> Result<(u64, u64)> {
    count_out_tree_for_permissions(conn, false)
}

/// Count of directory rows still needing metadata (only relevant with `--overwrite-dir`).
pub fn count_out_tree_for_permissions_dirs(conn: &Connection) -> Result<(u64, u64)> {
    count_out_tree_for_permissions(conn, true)
}

/// Phase-bar accounting for one `out_tree` slice of the permissions pass:
/// `(pending, done)` = rows still to apply, and rows already handled
/// (`AppliedMetadata` or `ErrorWhileApplyingMetadata`). Bar length =
/// `pending + done`, position = `done`, so a resumed run restarts where it
/// left off. Mirrors the listers' predicates.
fn count_out_tree_for_permissions(conn: &Connection, dirs: bool) -> Result<(u64, u64)> {
    // Directories are never marked Placed/errored (created up-front by
    // prepare_extraction_dir); NULL file_id ancestors (ensure_parent) are included.
    let (where_kind, params) = if dirs {
        (
            "AND o.flags & :is_dir != 0".to_string(),
            named_params! {
                ":applied": OutTreeFlag::AppliedMetadata.mask_i64(),
                ":err": OutTreeFlag::ErrorWhileApplyingMetadata.mask_i64(),
                ":is_dir": OutTreeFlag::IsDirectory.mask_i64(),
            },
        )
    } else {
        (
            "AND o.flags & :is_dir = 0 \
             AND o.flags & :placed != 0 \
             AND o.canonical_id = o.id".to_string(),
            named_params! {
                ":placed": OutTreeFlag::Placed.mask_i64(),
                ":applied": OutTreeFlag::AppliedMetadata.mask_i64(),
                ":err": OutTreeFlag::ErrorWhileApplyingMetadata.mask_i64(),
                ":is_dir": OutTreeFlag::IsDirectory.mask_i64(),
            },
        )
    };
    let pending: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM out_tree o
             WHERE (o.flags & :applied) = 0
               AND (o.flags & :err) = 0
               {where_kind}"
        ),
        params,
        |row| row.get(0),
    ).to_panic()?;
    let done: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM out_tree o
             WHERE (o.flags & :applied) != 0
                OR (o.flags & :err) != 0
               {where_kind}"
        ),
        params,
        |row| row.get(0),
    ).to_panic()?;
    Ok((pending as u64, done as u64))
}

/// Phase-bar accounting for the link-tree path: `(pending, done)` = canonical
/// files staged at the link source whose metadata still needs applying, and
/// those already handled. Mirrors `list_canonical_files_for_permissions`.
pub fn count_canonical_files_for_permissions(conn: &Connection) -> Result<(u64, u64)> {
    let pending: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files
         WHERE (flags & :at_dst) != 0
           AND (flags & :applied) = 0
           AND (flags & :err) = 0",
        named_params! {
            ":at_dst": FileFlag::AtLinkSource.mask_i64(),
            ":applied": FileFlag::AppliedMetadata.mask_i64(),
            ":err": FileFlag::ErrorWhileApplyingMetadata.mask_i64(),
        },
        |row| row.get(0),
    ).to_panic()?;
    let done: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files
         WHERE (flags & :at_dst) != 0
           AND ((flags & :applied) != 0
             OR (flags & :err) != 0)",
        named_params! {
            ":at_dst": FileFlag::AtLinkSource.mask_i64(),
            ":applied": FileFlag::AppliedMetadata.mask_i64(),
            ":err": FileFlag::ErrorWhileApplyingMetadata.mask_i64(),
        },
        |row| row.get(0),
    ).to_panic()?;
    Ok((pending as u64, done as u64))
}

/// (metadata applied, metadata with error)
///
/// `AppliedPermissions` ↔ every `out_tree` row referencing the file has
/// [`OutTreeFlag::AppliedMetadata`]. `ErrorWhileApplyingPermissions` ↔ at least one
/// row referencing the file has [`OutTreeFlag::ErrorWhileApplyingMetadata`].
pub fn apply_permissions_flags_to_files(conn: &Connection) -> Result<(u64, u64)> {
    // AppliedPermissions: only when ALL out_tree rows for the file are applied.
    let applied = conn.execute(
        "UPDATE files SET flags = flags | :file_applied
            WHERE files.id IN (SELECT file_id FROM out_tree)
                AND files.id NOT IN (SELECT file_id FROM out_tree WHERE flags & :out_applied = 0)",
        named_params! {
            ":file_applied": FileFlag::AppliedMetadata.mask_i64(),
            ":out_applied": OutTreeFlag::AppliedMetadata.mask_i64(),
        },
    ).to_panic()?;
    // ErrorWhileApplyingPermissions: ANY out_tree row for the file errored.
    let errored = conn.execute(
        "UPDATE files SET flags = flags | :file_error
            WHERE files.id IN (SELECT file_id FROM out_tree WHERE flags & :out_error != 0)",
        named_params! {
            ":file_error": FileFlag::ErrorWhileApplyingMetadata.mask_i64(),
            ":out_error": OutTreeFlag::ErrorWhileApplyingMetadata.mask_i64(),
        },
    ).to_panic()?;
    Ok((applied as u64, errored as u64))
}

/// List the canonical files in the link destination which were moved there successfully and set the PermissionsApplied flag (since t
pub fn list_canonical_files_for_permissions<R: SqlFileRow>(conn: &Connection, batch_size: u64) 
    -> Result<Vec<R>> {
    let cols = R::sql_columns(None);
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols} FROM files
         WHERE flags & :at_dst != 0
            AND flags & :perm_applied = 0
            AND flags & :perm_err = 0
            LIMIT :batch_size"
    )).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":at_dst": FileFlag::AtLinkSource.mask_i64(),
            ":perm_applied": FileFlag::AppliedMetadata.mask_i64(),
            ":perm_err": FileFlag::ErrorWhileApplyingMetadata.mask_i64(),
            ":batch_size": batch_size},
        |row| R::from_row(row, None)
    ).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// bool refers to `is_empty()` true -> empty, >0 errors false
pub fn ingest_apply_permission_out_tree_results(conn: &mut Connection, res: &Vec<(OutTreeId, bool)>)
                                                -> Result<u64> {
    with_transaction(conn, |i_conn| {
        for (id, is_empty) in res {
            if *is_empty {
                set_out_tree_flag(i_conn, *id, OutTreeFlag::AppliedMetadata, true)?;
            } else {
                set_out_tree_flag(i_conn, *id, OutTreeFlag::ErrorWhileApplyingMetadata, true)?;
            }
        }
        Ok(())
    }).to_panic()?;
    Ok(res.len() as u64)
}

/// bool refers to `is_empty()` true -> empty, >0 errors false
pub fn ingest_apply_permission_file_results(conn: &mut Connection, res: &Vec<(FileId, bool)>)
                                                -> Result<u64> {
    with_transaction(conn, |i_conn| {
        for (id, is_empty) in res {
            if *is_empty {
                set_file_flag(i_conn, *id, FileFlag::AppliedMetadata, true)?;
            } else {
                set_file_flag(i_conn, *id, FileFlag::ErrorWhileApplyingMetadata, true)?;
            }
        }
        Ok(())
    }).to_panic()?;
    Ok(res.len() as u64)
}