use rusqlite::{Connection, named_params};

use crate::db::common::{SqlFileRow, with_transaction};
use crate::db::flags::{FileFlag, OutTreeFlag, set_file_flag, set_out_tree_flag};
use crate::db::meta;
use crate::db::types::{FileId, OutTreeId, OutTreeRecord};
use crate::error::{Error, FileStatError, Result, ToPanic};

pub struct MaterializeResult {
    pub id: OutTreeId,
    pub placed: bool,
    pub conflict: bool,
    pub removed: bool,
    pub used_copy: bool,
}

/// Outcome of moving one canonical file into the link source.
pub type CopyOutcome = std::result::Result<(FileId, bool), (FileId, Error)>;

/// Function inserts the out_tref rows into the out_ref table
pub fn insert_ref_out_rows(conn: &Connection, pairs: &[(OutTreeId, i64)]) -> Result<()> {
    if pairs.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO ref_out (out_id, source_id)
         VALUES (:out_id, :source_id)",
    ).to_panic()?;
    for (out_id, source_id) in pairs {
        stmt.execute(named_params! {
            ":out_id": out_id.0,
            ":source_id": source_id,
        }).to_panic()?;
    }
    Ok(())
}

pub fn count_out_tree_rows(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM out_tree",
        [],
        |row| row.get(0)
    ).to_panic()?;
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
            |row| row.get(0)).to_panic()?,
        Some((placed, err)) => conn.query_row(
            &sql,
            named_params! {
                ":dir": OutTreeFlag::IsDirectory.mask_i64(),
                ":placed": placed,
                ":err": err,
            },
            |row| row.get(0)).to_panic()?,
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
            [],
            |row| row.get(0)).to_panic()?,
        Some((placed, err)) => conn.query_row(
            &sql,
            named_params! {
                ":placed": placed,
                ":err": err,
            },
            |row| row.get(0)).to_panic()?,
    };
    Ok(n as u64)
}

/// Function counts the number of entries in the out_tree table which are not dirs, files or unknown
/// canonical_id IS NULL (implied by mark canonical)
pub fn count_out_tree_others(conn: &Connection, materialized: Option<bool>) -> Result<u64> {
    let (mat_filter, mat_masks) = out_tree_materialized_filter(materialized);
    let sql = format!(
        "SELECT COUNT(*) FROM out_tree \
         WHERE canonical_id IS NULL {mat_filter}"
    );
    let n: i64 = match mat_masks {
        None => conn.query_row(
            &sql,
            [],
            |row| row.get(0),
        ).to_panic()?,
        Some((placed, err)) => conn.query_row(
            &sql,
            named_params! {
                ":placed": placed,
                ":err": err,
            },
            |row| row.get(0),
        ).to_panic()?,
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
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM ref_out", [], |row| row.get(0)).to_panic()?;
    Ok(n as u64)
}

pub fn dir_tree_is_built(conn: &Connection) -> Result<bool> {
    Ok(meta::get_dir_tree_built(conn)?.unwrap_or(false))
}

pub fn set_dir_tree_built(conn: &Connection) -> Result<()> {
    meta::set_dir_tree_built(conn, true)
}

pub fn list_canonical_files_for_move<R: SqlFileRow>(
    conn: &Connection, filter: bool, last_id: FileId, batch_size: u64)
    -> Result<Vec<R>> {
    debug_assert!(last_id.0 >= 0,
                  "INVARIANT ERROR: Only > 0 FileIds handed out, 0 minimum lower bound");

    let cols = R::sql_columns(None);
    let sql_filt = if filter {
        format!(" AND {}", crate::db::common::generate_archive_and_extract_filter(None))
    } else {
        String::new()
    };
    let mut stmt = conn.prepare(&format!("
        SELECT {cols} FROM files \
            WHERE flags & :extracted != 0
                AND flags & :moved = 0
                AND ftype = 'file'
                AND phase = 'rehashed'
                AND id > :last_id
                {sql_filt}
            ORDER BY id LIMIT :batch_size
        ")).to_panic()?;
    let results = stmt.query_map(
        named_params! {
            ":extracted": FileFlag::FileExtracted.mask_i64(),
            ":last_id": last_id.0,
            ":batch_size": batch_size,
            ":moved": FileFlag::AtLinkSource.mask_i64()
        },
        |r| R::from_row(r, None),
    ).to_panic()?;
    results.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// Shared WHERE selecting the **overall** link-tree move workload — the stable
/// set, independent of move progress (no `AtLinkSource` predicate). Used by
/// both `count_files_to_move` and `populate_canonical_move_queue`'s family
/// (mirrors `db/rehash.rs::files_to_rehash_where`).
fn files_to_move_where(filter: bool) -> String {
    let sql_filt = if filter {
        " AND {}".to_string()
            + crate::db::common::generate_archive_and_extract_filter(None).as_str()
    } else {
        String::new()
    };
    format!(
        "flags & :extracted != 0
             AND ftype = 'file'
             AND phase = 'rehashed'
             {sql_filt}"
    )
}

/// Count the **overall** link-tree move workload — canonical files that will be
/// moved regardless of whether they already have been. Stable across sessions,
/// so a resumed run's moved bar still reflects the full workload.
pub fn count_files_to_move(conn: &Connection, filter: bool) -> Result<u64> {
    let sql = format!("SELECT COUNT(*) AS count FROM files WHERE {}", files_to_move_where(filter));
    let count: i64 = conn.query_row(
        &sql,
        named_params! { ":extracted": FileFlag::FileExtracted.mask_i64() },
        |row| row.get("count"),
    ).to_panic()?;
    Ok(count as u64)
}

/// Of [`count_files_to_move`], how many are already at the link source — the
/// resume position of the moved bar (`pending` is the difference).
pub fn count_moved_files(conn: &Connection, filter: bool) -> Result<u64> {
    let sql = format!(
        "SELECT COUNT(*) AS count FROM files
         WHERE {} AND (flags & :moved) != 0",
        files_to_move_where(filter)
    );
    let count: i64 = conn.query_row(
        &sql,
        named_params! {
            ":extracted": FileFlag::FileExtracted.mask_i64(),
            ":moved": FileFlag::AtLinkSource.mask_i64(),
        },
        |row| row.get("count"),
    ).to_panic()?;
    Ok(count as u64)
}

/// Create the `canonical_move_queue` ordering table. Idempotent.
pub fn create_canonical_move_queue(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS canonical_move_queue (
             id      INTEGER PRIMARY KEY,
             file_id INTEGER NOT NULL UNIQUE REFERENCES files(id)
         )",
        [],
    ).to_panic()?;
    Ok(())
}

/// Populate `canonical_move_queue` with the stable set of files to move into
/// the link source, `size DESC, id` order (position = `row_number()`).
/// Idempotent: `INSERT OR IGNORE` (via `UNIQUE(file_id)`) keeps rows from an
/// earlier populate (resume). The predicate mirrors
/// `list_canonical_files_for_move`: already-moved (`AtLinkSource`) rows are not
/// queued.
pub fn populate_canonical_move_queue(conn: &Connection, filter: bool) -> Result<u64> {
    let sql_filt = if filter {
        format!("AND {}", crate::db::common::generate_archive_and_extract_filter(None))
    } else {
        String::new()
    };
    let sql = format!(
        "INSERT OR IGNORE INTO canonical_move_queue (id, file_id)
         SELECT row_number() OVER (ORDER BY f.size DESC, f.id), f.id
         FROM files AS f
         WHERE f.flags & :extracted != 0
             AND f.flags & :moved = 0
             AND f.ftype = 'file'
             AND f.phase = 'rehashed'
             {sql_filt}"
    );
    let n = conn.execute(
        &sql,
        named_params! {
            ":extracted": FileFlag::FileExtracted.mask_i64(),
            ":moved": FileFlag::AtLinkSource.mask_i64(),
        },
    ).to_panic()?;
    Ok(n as u64)
}

/// Next slice of still-pending files to move, in `canonical_move_queue`
/// (size-DESC) order. Rejects rows already flagged `AtLinkSource` (moved on a
/// previous session), so a resume skips done files; the queue only supplies the
/// ordering. The returned `u64` is the queue position, letting the caller
/// advance the read `index` across slices.
pub fn pull_canonical_move_queue<R: SqlFileRow>(
    conn: &Connection, index: u64, limit: u64)
    -> Result<Vec<(u64, R)>> {
    let cols = R::sql_columns(Some("files"));
    let sql = format!(
        "SELECT canonical_move_queue.id AS pos, {cols}
         FROM files JOIN canonical_move_queue ON canonical_move_queue.file_id = files.id
         WHERE canonical_move_queue.id > :index
           AND (files.flags & :moved) = 0
         ORDER BY canonical_move_queue.id
         LIMIT :limit"
    );
    let mut stmt = conn.prepare(&sql).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":index": index as i64,
            ":moved": FileFlag::AtLinkSource.mask_i64(),
            ":limit": limit,
        },
        |r: &rusqlite::Row<'_>| {
            let pos = r.get::<_, i64>("pos")? as u64;
            let record = R::from_row(r, Some("files"))?;
            Ok((pos, record))
        },
    ).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// Drop the `canonical_move_queue` ordering table. Idempotent. Call on the
/// phase-success path only; leave it in place across interrupts/resumes.
pub fn drop_canonical_move_queue(conn: &Connection) -> Result<()> {
    conn.execute("DROP TABLE IF EXISTS canonical_move_queue", []).to_panic()?;
    Ok(())
}

/// Create the `materialize_queue` ordering table. Idempotent.
pub fn create_materialize_queue(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS materialize_queue (
             id           INTEGER PRIMARY KEY,
             out_tree_id  INTEGER NOT NULL UNIQUE REFERENCES out_tree(id)
         )",
        [],
    ).to_panic()?;
    Ok(())
}

/// Populate `materialize_queue` with the canonical file rows of the out_tree in
/// `size DESC, out_tree id` order (position = `row_number()`). Idempotent:
/// `INSERT OR IGNORE` keeps rows from an earlier populate (resume). The
/// predicate mirrors `list_out_tree_for_materialization`'s stable set; the pull
/// re-filters to rows not yet placed.
pub fn populate_materialize_queue(conn: &Connection) -> Result<u64> {
    let sql = "
        INSERT OR IGNORE INTO materialize_queue (id, out_tree_id)
        SELECT row_number() OVER (ORDER BY f.size DESC, o.id), o.id
        FROM out_tree AS o
        JOIN files AS f ON o.file_id = f.id
        WHERE o.canonical_id = o.id
            AND f.ftype = 'file'";
    let n = conn.execute(sql, []).to_panic()?;
    Ok(n as u64)
}

/// Next slice of still-pending canonical file rows, in `materialize_queue`
/// (size-DESC) order. `R` reads the dedup canonical (`f.canonical_id = c.id`).
/// Rejects rows already `Placed` or `ErrorWhilePlace` (done on a previous
/// session); the queue only supplies the ordering. Returns
/// `(queue_position, canonical_record, out_tree_record)` triples.
pub fn pull_materialize_queue<R: SqlFileRow>(
    conn: &Connection, index: u64, limit: u64)
    -> Result<Vec<(u64, R, OutTreeRecord)>> {
    let file_cols = R::sql_columns(Some("c"));
    let out_cols = OutTreeRecord::sql_columns(Some("o"));
    let sql = format!(
        "SELECT materialize_queue.id AS pos, {file_cols}, {out_cols}
         FROM materialize_queue
         JOIN out_tree AS o ON o.id = materialize_queue.out_tree_id
         JOIN files AS f ON o.file_id = f.id
         JOIN files AS c ON f.canonical_id = c.id
         WHERE materialize_queue.id > :index
           AND (o.flags & :placed) = 0
           AND (o.flags & :err) = 0
         ORDER BY materialize_queue.id
         LIMIT :limit"
    );
    let mut stmt = conn.prepare(&sql).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":index": index as i64,
            ":placed": OutTreeFlag::Placed.mask_i64(),
            ":err": OutTreeFlag::ErrorWhilePlace.mask_i64(),
            ":limit": limit,
        },
        |row| {
            let pos = row.get::<_, i64>("pos")? as u64;
            let fr = R::from_row(row, Some("c"))?;
            let or = OutTreeRecord::from_sql(row, Some("o"))?;
            Ok((pos, fr, or))
        },
    ).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// Drop the `materialize_queue` ordering table. Idempotent. Call on the
/// phase-success path only; leave it in place across interrupts/resumes.
pub fn drop_materialize_queue(conn: &Connection) -> Result<()> {
    conn.execute("DROP TABLE IF EXISTS materialize_queue", []).to_panic()?;
    Ok(())
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
    ")).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":last_id": last_id.0,
            ":batch_size": batch_size},
        |row| {
            let sr = R::from_row(row, Some("c"))?;
            let or = OutTreeRecord::from_sql(row, Some("o"))?;
            Ok((sr, or))
        },
    ).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

pub fn list_out_tree_for_hardlinks<R: SqlFileRow>(
    conn: &Connection, last_id: &OutTreeId, batch_size: u64)
    -> Result<Vec<(R, OutTreeRecord, OutTreeRecord)>> {
    let can_cols = R::sql_columns(Some("f"));
    let tgt_cols = OutTreeRecord::sql_columns(Some("c"));
    let out_cols = OutTreeRecord::sql_columns(Some("o"));
    let mut stmt = conn.prepare(&format!("
        SELECT {can_cols}, {tgt_cols}, {out_cols}
        FROM out_tree AS o
        JOIN out_tree AS c ON o.canonical_id = c.id
        JOIN files AS f ON o.file_id = f.id
        WHERE o.id > :last_id
            AND f.ftype = 'file'
            AND o.canonical_id != o.id
            AND o.canonical_id IS NOT NULL
        ORDER BY o.id
        LIMIT :batch_size
    ")).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":last_id": last_id.0,
            ":batch_size": batch_size},
        |row| {
            let fr = R::from_row(row, Some("f"))?;
            let sr = OutTreeRecord::from_sql(row, Some("c"))?;
            let or = OutTreeRecord::from_sql(row, Some("o"))?;
            Ok((fr, sr, or))
        },
    ).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
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
    ")).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":last_id": last_id.0,
            ":batch_size": batch_size},
        |row| {
            let sr = R::from_row(row, Some("e"))?;
            let or = OutTreeRecord::from_sql(row, Some("o"))?;
            Ok((sr, or))
        },
    ).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// List all rows (which aren't directories) which remain to be linked into place
pub fn list_out_tree_for_linking<R: SqlFileRow>(
    conn: &Connection, batch_size: u64, pending: bool)
    -> Result<Vec<(R, OutTreeRecord)>> {
    let file_cols = R::sql_columns(Some("can"));
    let out_cols = OutTreeRecord::sql_columns(Some("o"));
    let filter_placed = if pending {
        " AND o.flags & :placement = 0
          AND o.flags & :place_error = 0"
    } else {
        " AND (o.flags & :placement != 0
               OR o.flags & :place_error != 0)"
    };
    let mut stmt = conn.prepare(&format!("
        SELECT {file_cols}, {out_cols}
        FROM files AS can
        JOIN files AS ent ON can.id = ent.canonical_id
        JOIN out_tree AS o ON f.id = o.file_id
        WHERE f.ftype NOT IN ('dir', 'unknown')
            {filter_placed}
            AND f.flags & :moved != 0
        ORDER BY o.id LIMIT :batch_size
        ")).to_panic()?;
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
        },
    ).to_panic()?;
    results.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
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
        },
    ).to_panic()?;
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
    ).to_panic()?;

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
    ).to_panic()?;

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
    ).to_panic()?;

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
    ).to_panic()?;

    Ok((updated + updated2) as u64)
}

/// (placed, reflinked, conflict, removed_previous, errored, skipped)
pub fn apply_flags_to_files(conn: &Connection) -> Result<(u64, u64, u64, u64, u64, u64)> {
    // Placed if all are placed
    let placed = conn.execute(
        "UPDATE files SET flags = flags | :file_placed
            WHERE files.id IN (SELECT file_id FROM out_tree)
                AND files.id NOT IN (SELECT file_id FROM out_tree WHERE flags & :out_placed = 0)",
        named_params! {
            ":file_placed": FileFlag::Placed.mask_i64(),
            ":out_placed": OutTreeFlag::Placed.mask_i64(),
        },
    ).to_panic()?;
    // Reflinked if all are reflink
    let reflinked = conn.execute(
        "UPDATE files SET flags = flags | :file_reflink
            WHERE files.id IN (SELECT file_id FROM out_tree)
                AND files.id NOT IN (SELECT file_id FROM out_tree WHERE flags & :out_reflink = 0)",
        named_params! {
        ":file_reflink": FileFlag::UsedRefLink.mask_i64(),
        ":out_reflink": OutTreeFlag::UsedRefLink.mask_i64()
        },
    ).to_panic()?;
    // Conflicts if any conflict happened.
    let conflict = conn.execute(
        "UPDATE files SET flags = flags | :file_conflict
            WHERE files.id IN (SELECT file_id FROM out_tree WHERE flags & :out_conflict = 0)",
        named_params! {
        ":file_conflict": FileFlag::Conflict.mask_i64(),
        ":out_conflict": OutTreeFlag::Conflict.mask_i64()
        },
    ).to_panic()?;
    // Removed previous
    let removed_previous = conn.execute(
        "UPDATE files SET flags = flags | :file_removed
            WHERE files.id IN (SELECT file_id FROM out_tree WHERE flags & :out_removed = 0)",
        named_params! {
        ":file_removed": FileFlag::Conflict.mask_i64(),
        ":out_removed": OutTreeFlag::Conflict.mask_i64()
        },
    ).to_panic()?;
    // Errored if any are errored
    let errored = conn.execute(
        "UPDATE files SET flags = flags | :file_error
            WHERE files.id IN (SELECT file_id FROM out_tree WHERE flags & :out_error = 0)",
        named_params! {
        ":file_error": FileFlag::ErrorWhilePlacing.mask_i64(),
        ":out_error": OutTreeFlag::ErrorWhilePlace.mask_i64()
        },
    ).to_panic()?;

    // Skipped Element
    let skipped_elements: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM out_tree \
        WHERE flags & :placed = 0 AND flags & :conflict != 0",
        named_params! {
        ":placed": OutTreeFlag::Placed.mask_i64(),
        ":conflict": OutTreeFlag::Conflict.mask_i64()
        },
        |row| row.get(0),
    ).to_panic()?;
    Ok((
        placed as u64,
        reflinked as u64,
        conflict as u64,
        removed_previous as u64,
        errored as u64,
        skipped_elements as u64))
}

/// Take a batch of link tree results and ingest themn into the database within a single transaction
pub fn ingest_results_link_tree(conn: &mut Connection, results: &Vec<(OutTreeId, Option<FileStatError>)>)
                                -> Result<u64> {
    with_transaction(conn, |i_conn| {
        for (id, err) in results {
            match err {
                None => { set_out_tree_flag(i_conn, *id, OutTreeFlag::Placed, true)?; }
                Some(_e) => { set_out_tree_flag(i_conn, *id, OutTreeFlag::ErrorWhilePlace, true)?; }
            }
        }
        Ok(())
    }).to_panic()?;
    Ok(results.len() as u64)
}

#[cfg(debug_assertions)]
fn validate_materialize_result(m: &MaterializeResult) -> () {
    match (m.placed, m.conflict, m.removed, m.used_copy) {
        // No conflict
        (true, false, false, _) => (),
        // Conflict, skip
        (false, true, false, false) => (),
        // Conflict, place
        (true, true, _, _) => (),
        _ => panic!("INVARIANT FAILED: Impossible flag constellation returned from placement "),
    }
}

/// Ingest a batch of MaterializeResults into the db within a single transaction
pub fn ingest_materialize_results(
    conn: &mut Connection,
    results: &Vec<std::result::Result<MaterializeResult, (OutTreeId, Error)>>,
    is_hardlink: bool,
    set_reflink: bool)
    -> Result<()> {
    with_transaction(conn, |i_conn| {
        for result in results {
            match result {
                Err((_id, Error::Interrupted)) => (),
                Err((id, Error::FileStat(_))) => {
                    set_out_tree_flag(i_conn, *id, OutTreeFlag::ErrorWhilePlace, true)?;
                },
                Err((_id, other)) => {
                    panic!("INVARIANT FAILED: Only Io and Interrupted errors expected, got {}",
                           other
                    );
                }
                Ok(suc) => {
                    if cfg!(debug_assertions) {
                        validate_materialize_result(&suc);
                    }
                    set_out_tree_flag(i_conn, suc.id, OutTreeFlag::Placed, suc.placed)?;
                    set_out_tree_flag(i_conn, suc.id, OutTreeFlag::RemovedPrevious, !suc.removed)?;
                    set_out_tree_flag(i_conn, suc.id, OutTreeFlag::Conflict, suc.conflict)?;
                    if is_hardlink {
                        set_out_tree_flag(i_conn, suc.id, OutTreeFlag::IsHardlink, true)?;
                    }
                    if set_reflink {
                        set_out_tree_flag(i_conn, suc.id, OutTreeFlag::UsedRefLink, !suc.used_copy)?;
                    }
                }
            }
        }
        Ok(())
    }).to_panic()?;
    Ok(())
}

/// Move the results of the copy canonical files to source directory into the database.
pub fn ingest_copy_results(conn: &mut Connection, results: &Vec<CopyOutcome>) -> Result<u64> {
    with_transaction(conn, |i_conn| {
        for result in results {
            match result {
                Ok((id, is_copy)) => {
                    set_file_flag(i_conn, *id, FileFlag::AtLinkSource, true)?;
                    set_file_flag(i_conn, *id, FileFlag::UsedRefLink, !is_copy)?;
                }
                // Interrupted is swallowed by the loop's drain; the row stays pending.
                Err((_, Error::Interrupted)) => (),
                Err((id, Error::FileStat(_))) => {
                    set_file_flag(i_conn, *id, FileFlag::ErrorWhilePlacing, true)?;
                }
                Err((_id, other)) => panic!(
                    "INVARIANT FAILED: Return type violates contract. Encountered error {other}"
                ),
            }
        }
        Ok(())
    }).to_panic()?;
    Ok(results.len() as u64)
}