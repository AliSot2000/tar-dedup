use rusqlite::{Connection, named_params};

use crate::db::common::SqlFileRow;
use crate::db::flags::OutTreeFlags;
use crate::db::flags::{FileFlag, OutTreeFlag};
use crate::db::meta;
use crate::db::types::{FileId, NewOutTreeRow, OutTreeId, OutTreeRecord,
};
use crate::error::Result;

/// Convenience implementations for the OutTreeRecord, function to parse sql rows to records and
/// generate the columns to select
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
            canonical_id: OutTreeId(row.get(format!("{upx}canonical_id").as_str())?)
        })
    }

    fn sql_columns(prefix: Option<&str>) -> String {
        match prefix {
            None => "id, abs_path, file_id, flags, canonical_id".to_string(),
            Some(p) => format!("\
            {p}.id AS \"{p}.id\",
            {p}.abs_path AS \"{p}.abs_path\",
            {p}.file_id AS \"{p}.file_id\",
            {p}.flags AS \"{p}.flags\",
            {p}.canonical_id AS \"{p}.canonical_id\"
            "
            )
        }
    }
}

/// Function iterates through the files table to find all entries which should get materialized
/// based on filtering (include, exclude filter) and on file type. Additionally, a source can be
/// added s.t. only the files which are covered by this source are returned.
pub fn list_materialized_entries<R: SqlFileRow>(
    conn: &Connection,
    last_id: Option<FileId>,
    batch_size: u64,
    source_id: Option<i64>,
    only_dirs: Option<bool>,
) -> Result<Vec<R>> {
    let last_id = last_id.unwrap_or(FileId(0)).0;
    let columns = R::sql_columns(Some("f"));
    let filter_dir = match only_dirs {
        None => "",
        Some(true) => " AND f.ftype = 'dir' ",
        Some(false) => " AND f.ftype NOT IN ('dir', 'unknown') " // INFO: ftype IS NOT NULL!
    };
    let sql = match source_id {
        Some(_) => &format!("SELECT {columns}
            FROM files f
            WHERE f.id > :last_id
              AND f.include_reason < 0
              AND f.exclude_reason = 0
              AND f.id IN (SELECT file_id FROM ref WHERE source_id = :source_id)
              {filter_dir}
            ORDER BY f.id
            LIMIT :batch_size"),
        None => &format!("SELECT {columns}
            FROM files f
            WHERE f.id > :last_id
              AND f.include_reason < 0
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
    let rows = stmt.query_map(
        params,
        |r| R::from_row(r, None)
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
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

/// Function inserts the out_tref rows into the out_ref table
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

/// Function lists all elements of the out_tree in batches
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
    let cols = OutTreeRecord::sql_columns(Some("o"));
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols}
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
        |r | OutTreeRecord::from_sql(r, None)
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

/// Function counts the number of entries marked as canonical inside the out_tree
pub fn count_out_tree_canonicals(conn: &Connection, materialized: Option<bool>) -> Result<u64> {
    let (mat_filter, mat_masks) = out_tree_materialized_filter(materialized);
    let sql = format!(
        "SELECT COUNT(*) FROM out_tree \
         WHERE flags & :dir = 0 AND canonical_id = id {mat_filter}");
    let n: i64 = match mat_masks {
        None => conn.query_row(
            &sql,
            named_params! { ":dir": OutTreeFlag::IsDirectory.mask_i64() },
            |row| row.get(0))?,
        Some((placed, err)) => conn.query_row(
            &sql,
            named_params! {
                ":dir": OutTreeFlag::IsDirectory.mask_i64(),
                ":placed": placed,
                ":err": err,
            },
            |row| row.get(0))?,
    };
    Ok(n as u64)
}

/// Function counts the number of entries in the out_tree table which are files and not canonical:
/// canonical_id != id
pub fn count_out_tree_hardlinks(conn: &Connection, materialized: Option<bool>) -> Result<u64> {
    let (mat_filter, mat_masks) = out_tree_materialized_filter(materialized);
    let sql = format!(
        "SELECT COUNT(*) FROM out_tree \
         WHERE canonical_id != id AND canonical_id IS NOT NULL {mat_filter}");
    let n: i64 = match mat_masks {
        None => conn.query_row(
            &sql,
            named_params! { ":dir": OutTreeFlag::IsDirectory.mask_i64() },
            |row| row.get(0))?,
        Some((placed, err)) => conn.query_row(
            &sql,
            named_params! {
                ":dir": OutTreeFlag::IsDirectory.mask_i64(),
                ":placed": placed,
                ":err": err,
            },
            |row| row.get(0))?,
    };
    Ok(n as u64)
}

/// Function counts the number of entries in the out_tree table which are not dirs, files or unknown
/// canonical_id IS NULL (implied by mark canonical)
pub fn count_out_tree_others(conn: &Connection, materialized: Option<bool>) -> Result<u64> {
    let (mat_filter, mat_masks) = out_tree_materialized_filter(materialized);
    let sql = format!(
        "SELECT COUNT(*) FROM out_tree \
         WHERE canonical_id IS NULL {mat_filter}");
    let n: i64 = match mat_masks {
        None => conn.query_row(
            &sql,
            named_params! { ":dir": OutTreeFlag::IsDirectory.mask_i64() },
            |row| row.get(0))?,
        Some((placed, err)) => conn.query_row(
            &sql,
            named_params! {
                ":dir": OutTreeFlag::IsDirectory.mask_i64(),
                ":placed": placed,
                ":err": err,
            },
            |row| row.get(0))?,
    };
    Ok(n as u64)
}

/// Build the `materialized` WHERE fragment and the `(placed, err)` masks it
/// references. `None` → no filter (No masks bound). `Some(true)` → at least one
/// of Placed / ErrorWhilePlace set. `Some(false)` → neither set.
fn out_tree_materialized_filter(materialized: Option<bool>) -> (String, Option<(i64, i64)>) {
    match materialized {
        None => (String::new(), None),
        Some(true) => (
            " AND ((flags & :placed) != 0 OR (flags & :err) != 0)".to_string(),
            Some((OutTreeFlag::Placed.mask_i64(), OutTreeFlag::ErrorWhilePlace.mask_i64())),
        ),
        Some(false) => (
            " AND ((flags & :placed) = 0 AND (flags & :err) = 0)".to_string(),
            Some((OutTreeFlag::Placed.mask_i64(), OutTreeFlag::ErrorWhilePlace.mask_i64())),
        ),
    }
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
    let sql_filt = if filter { " AND include_reason < 0 AND exclude_reason = 0" } else { "" };
    let mut stmt = conn.prepare(&format!("\
        SELECT {cols} FROM files \
            WHERE flags & :extracted != 0
                AND flags & :moved = 0
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

pub fn list_out_tree_for_materialization<R: SqlFileRow>(
    conn: &Connection, last_id: &OutTreeId, batch_size: u64)
    -> Result<Vec<(R, OutTreeRecord)>> {
    let file_cols = R::sql_columns(Some("c"));
    let out_cols = OutTreeRecord::sql_columns(Some("o"));
    let mut stmt = conn.prepare(&format!("
        SELECT {file_cols}, {out_cols}
        FROM out_tree AS o
        JOIN files as f ON o.file_id = f.id
        JOIN files as c ON f.canonical_id = c.id
        WHERE o.id > :last_id
            AND f.ftype = 'file'
            AND o.canonical_id = o.id
        ORDER BY o.id
        LIMIT :batch_size
    "))?;
    let rows = stmt.query_map(
        named_params! {
            ":last_id": last_id.0,
            ":batch_size": batch_size},
        |row| {
            let sr = R::from_row(row, Some("c"))?;
            let or = OutTreeRecord::from_sql(row, Some("o"))?;
            Ok((sr, or))
        }
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn list_out_tree_for_hardlinks(
    conn: &Connection, last_id: &OutTreeId, batch_size: u64)
    -> Result<Vec<(OutTreeRecord, OutTreeRecord)>> {
    let tgt_cols = OutTreeRecord::sql_columns(Some("c"));
    let out_cols = OutTreeRecord::sql_columns(Some("o"));
    let mut stmt = conn.prepare(&format!("
        SELECT {tgt_cols}, {out_cols}
        FROM out_tree AS o
        JOIN out_tree AS c ON o.canonical_id = c.id
        JOIN files AS f ON o.file_id = f.id
        WHERE o.id > :last_id
            AND f.ftype = 'file'
            AND o.canonical_id != o.id
            AND o.canonical_id IS NOT NULL
        ORDER BY o.id
        LIMIT :batch_size
    "))?;
    let rows = stmt.query_map(
        named_params! {
            ":last_id": last_id.0,
            ":batch_size": batch_size},
        |row| {
            let sr = OutTreeRecord::from_sql(row, Some("c"))?;
            let or = OutTreeRecord::from_sql(row, Some("o"))?;
            Ok((sr, or))
        }
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn list_out_tree_others<R: SqlFileRow>(conn: &Connection, last_id: &OutTreeId, batch_size: u64)
    -> Result<Vec<(R, OutTreeRecord)>> {
    let tgt_cols = R::sql_columns(Some("e"));
    let out_cols = OutTreeRecord::sql_columns(Some("o"));
    let mut stmt = conn.prepare(&format!("
        SELECT {tgt_cols}, {out_cols}
        FROM out_tree AS o
        JOIN files AS e ON o.file_id = e.id
        WHERE o.id > :last_id
            AND e.ftype NOT IN ('file', 'dir', 'unknown')
            AND o.canonical_id IS NULL
        ORDER BY o.id
        LIMIT :batch_size
    "))?;
    let rows = stmt.query_map(
        named_params! {
            ":last_id": last_id.0,
            ":batch_size": batch_size},
        |row| {
            let sr = R::from_row(row, Some("e"))?;
            let or = OutTreeRecord::from_sql(row, Some("o"))?;
            Ok((sr, or))
        }
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// List all rows (which aren't directories) which remain to be linked into place
pub fn list_out_tree_for_linking<R: SqlFileRow>(
    conn: &Connection, batch_size: u64, pending: bool)
    -> Result<Vec<(R, OutTreeRecord)>> {
    let file_cols = R::sql_columns(Some("can"));
    let out_cols = OutTreeRecord::sql_columns(Some("o"));
    let filter_placed = if pending {
        " AND o.flags & :placement = 0 \
          AND o.flags & :place_error = 0"
    } else {
        " AND (o.flags & :placement != 0 \
               OR o.flags & :place_error != 0)"
    };
    let mut stmt = conn.prepare(&format!("\
        SELECT {file_cols}, {out_cols} \
        FROM files AS can \
        JOIN files AS ent ON can.id = ent.canonical_id \
        JOIN out_tree AS o ON f.id = o.file_id \
        WHERE f.ftype NOT IN ('dir', 'unknown') \
            {filter_placed} \
            AND f.flags & :moved != 0 \
        ORDER BY o.id LIMIT :batch_size
        "))?;
    let results = stmt.query_map(
        named_params! {
            ":placement": OutTreeFlag::Placed.mask_i64(),
            ":moved": FileFlag::AtLinkSource.mask_i64(),
            ":batch_size": batch_size,
            ":place_error": OutTreeFlag::ErrorWhilePlace.mask_i64(),
        },
        |row| {
            let sr = R::from_row(row, Some("can"))?;
            let or = OutTreeRecord::from_sql(row, Some("o"))?;
            Ok((sr, or))
        }
    )?;
    results.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Mark all entries in the out_tree which are files (iff !dir) and mark them self-canonical.
pub fn mark_all_canonical(conn: &Connection) -> Result<u64> {
    //         SET flags = flags | :canonical | :walked \
    let update = conn.execute(
        "UPDATE out_tree \
        SET canonical_id = id \
        WHERE canonical_id IS NULL AND flags & :dir = 0",
        named_params! {
            // ":canonical": OutTreeFlag::IsCanonical.mask_i64(),
            // ":walked" : OutTreeFlag::EntryWalked.mask_i64(),
            ":dir": OutTreeFlag::IsDirectory.mask_i64(),
        })?;
   Ok(update as u64)
}

/// Set one file as the canonical in the out_tree given hardlink groups (dev, inode)
/// The (dev, inode) groups are formed across the entire external materialization tree.
pub fn mark_global_canonical(conn: &Connection) -> Result<u64> {
    // Step 1: elect one out_tree row per (dev, inode) group as the canonical
    // output row (self-link), using the group's dedup content canonical.
    let updated = conn.execute(
        "UPDATE out_tree SET canonical_id = id WHERE id IN
            (SELECT MIN(out.id)
             FROM files AS can
             JOIN files AS tree ON can.id = tree.canonical_id
             JOIN out_tree AS out ON tree.id = out.file_id
             WHERE tree.ftype = 'file'
               AND out.canonical_id IS NULL
               AND can.canonical_id = can.id
               AND can.dev IS NOT NULL AND can.inode IS NOT NULL
             GROUP BY can.dev, can.inode)",
        [],
    )?;

    // Step 2: point every other out_tree row at the elected canonical row of
    // its (dev, inode) group. The correlation is via the group's dedup
    // content canonical (files.canonical_id = files.id) so members link to a
    // real file payload of that group, not to an arbitrary same-inode row.
    let updated2 = conn.execute(
        "UPDATE out_tree SET canonical_id = (
            SELECT MIN(w.id)
            FROM out_tree AS w
            JOIN files AS wf ON wf.id = w.file_id
            JOIN files AS mf ON mf.id = out_tree.file_id
            WHERE w.canonical_id = w.id
              AND wf.canonical_id = wf.id
              AND mf.dev = wf.dev
              AND mf.inode = wf.inode
              AND wf.ftype = 'file'
         )
         WHERE out_tree.canonical_id IS NULL
           AND EXISTS (
               SELECT 1
               FROM out_tree AS w
               JOIN files AS wf ON wf.id = w.file_id
               JOIN files AS mf ON mf.id = out_tree.file_id
               WHERE w.canonical_id = w.id
                 AND wf.canonical_id = wf.id
                 AND mf.dev = wf.dev
                 AND mf.inode = wf.inode
                 AND wf.ftype = 'file'
           )",
        [],
    )?;

    Ok((updated + updated2) as u64)
}

/// Set one file as the canonical in the out_tree given hardlink groups (dev, inode)
/// The (dev, inode) groups are formed across the subtree which is induced by the source_id.
/// If multiple sources induce the same (or partially the same) tree, previously captured / marked
/// those are not considered for canonical computation. E.g.
///
/// Source A covers:
/// /path/to/dir
/// Source B Covers:
/// /path/to/dir/subdir
///
/// Suppose the following links
/// /path/to/dir/link_a
/// /path/to/dir/subdir/link_b
/// /path/to/dir/subdir/link_c
///
/// If A is marked before B, link_a, link_b and link_c are linked together
/// If B is marked before A, link_b, link_c are a group and link_a is a separate (non-linked) file.
pub fn mark_source_canonical(conn: &Connection, source_id: i64) -> Result<u64> {
    // Step 1: elect one out_tree row per (dev, inode) group as the canonical
    // output row (self-link), using the group's dedup content canonical.
    let updated = conn.execute(
        "UPDATE out_tree SET canonical_id = id WHERE id IN
            (SELECT MIN(out.id)
             FROM files AS can
             JOIN files AS tree ON can.id = tree.canonical_id
             JOIN out_tree AS out ON tree.id = out.file_id
             JOIN ref_out ON ref_out.out_id = out.id
             WHERE tree.ftype = 'file'
               AND out.canonical_id IS NULL
               AND can.canonical_id = can.id
               AND can.dev IS NOT NULL AND can.inode IS NOT NULL
               AND ref_out.source_id = :source_id
             GROUP BY can.dev, can.inode)",
        named_params! { ":source_id": source_id },
    )?;

    // Step 2: point every other out_tree row at the elected canonical row of
    // its (dev, inode) group. The correlation is via the group's dedup
    // content canonical (files.canonical_id = files.id) so members link to a
    // real file payload of that group, not to an arbitrary same-inode row.
    let updated2 = conn.execute(
        "UPDATE out_tree SET canonical_id = (
                SELECT MIN(w.id)
                FROM out_tree AS w
                JOIN files AS wf ON wf.id = w.file_id
                JOIN files AS mf ON mf.id = out_tree.file_id
                WHERE w.canonical_id = w.id
                    AND wf.canonical_id = wf.id
                    AND mf.dev = wf.dev
                    AND mf.inode = wf.inode
                    AND wf.ftype = 'file'
                )
            WHERE out_tree.canonical_id IS NULL
                AND EXISTS (
                    SELECT 1
                    FROM ref_out AS ro
                    WHERE ro.out_id = out_tree.id
                        AND ro.source_id = :source_id
                )
                AND EXISTS (
                    SELECT 1
                    FROM out_tree AS w
                    JOIN files AS wf ON wf.id = w.file_id
                    JOIN files AS mf ON mf.id = out_tree.file_id
                    WHERE w.canonical_id = w.id
                        AND wf.canonical_id = wf.id
                        AND mf.dev = wf.dev
                        AND mf.inode = wf.inode
                        AND wf.ftype = 'file'
                )",
        named_params! { ":source_id": source_id },
    )?;

    Ok((updated + updated2) as u64)
}

/// (placed, reflinked, errored, skipped)
pub fn apply_flags_to_files(conn: &Connection) -> Result<(u64, u64, u64, u64)> {
    // Placed if all are placed
    let placed = conn.execute(
        "UPDATE files SET flags = flags | :file_placed
            WHERE files.id IN (SELECT file_id FROM out_tree)
                AND files.id NOT IN (SELECT file_id FROM out_tree WHERE flags & :out_placed = 0)",
        named_params! {
            ":file_placed": FileFlag::Placed.mask_i64(),
            ":out_placed": OutTreeFlag::Placed.mask_i64(),
        },
    )?;
    // Reflinked if all are reflink
    let reflinked = conn.execute(
        "UPDATE files SET flags = flags | :file_reflink
            WHERE files.id IN (SELECT file_id FROM out_tree)
                AND files.id NOT IN (SELECT file_id FROM out_tree WHERE flags & :out_reflink = 0)",
        named_params! {
        ":file_reflink": FileFlag::UsedRefLink.mask_i64(),
        ":out_reflink": OutTreeFlag::UsedRefLink.mask_i64()
        },
    )?;
    // Errored if any are errored
    let errored = conn.execute(
        "UPDATE files SET flags = flags | :file_error
            WHERE files.id IN (SELECT file_id FROM out_tree WHERE flags & :out_error = 0)",
        named_params! {
        ":file_reflink": FileFlag::ErrorWhilePlacing.mask_i64(),
        ":out_reflink": OutTreeFlag::ErrorWhilePlace.mask_i64()
        },
    )?;
    // Dkipped if any are skipped.
    let skipped = conn.execute(
        "UPDATE files SET flags = flags | :file_skipped
            WHERE files.id IN (SELECT file_id FROM out_tree WHERE flags & :out_skipped = 0)",
        named_params! {
        ":file_skipped": FileFlag::Skipped.mask_i64(),
        ":out_skipped": OutTreeFlag::Skipped.mask_i64()
        },
    )?;
    Ok((placed as u64, reflinked as u64, errored as u64, skipped as u64))
}