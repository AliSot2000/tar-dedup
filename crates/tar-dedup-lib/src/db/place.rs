use rusqlite::{Connection, OptionalExtension, named_params};

use crate::db::common::{SqlFileRow, with_transaction};
use crate::db::flags::{FileFlag, OutTreeFlag, set_file_flag, set_out_tree_flag};
use crate::db::meta;
use crate::db::types::{FileId, OutTreeId, OutTreeRecord};
use crate::error::{Error, FileStatError, Result, ToPanic};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaterializeResult {
    pub id: OutTreeId,
    pub placed: bool,
    pub conflict: bool,
    pub removed: bool,
    pub used_copy: bool,
}

/// Outcome of materializing one output entry: the flags to ingest, plus an
/// optional real error. `Some(Error::Interrupted)` marks a force-abort mid-copy,
/// which ingest leaves pending (sets no flags). The `None` channel sentinel is
/// reserved for worker shutdown and never reaches ingest.
#[derive(Debug)]
pub struct MaterializeOutcome {
    pub result: MaterializeResult,
    pub error: Option<Error>,
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
    // Exclude the reserved id=0 "missing link target" marker row.
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM out_tree WHERE id > 0",
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
         WHERE canonical_id IS NULL AND id > 0 {mat_filter}"
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
        format!(" AND {}", crate::db::common::generate_archive_and_extract_filter(None))
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

/// Next slice of extracted canonical payload files eligible for removal, ordered
/// by `files.id` (the file id doubles as the read cursor — `WHERE id >
/// :last_id`). Unlike `pull_canonical_move_queue`, this reads the `files` table
/// directly and needs no `canonical_move_queue` companion table. Each returned
/// `(id, record)` pairs the page index with a self-canonical regular file whose
/// digest is known (`content_id()` = `Some`) and whose own self-canonical
/// out-tree row(s) have all settled on a terminal flag (`OUT_TERMINAL`) — i.e.
/// exactly the rows `cleanup_stage` may delete. Rows already marked
/// `FileFlag::PayloadRemoved` are excluded (idempotent sweep).
pub fn pull_extracted_cache_files<R: SqlFileRow>(
    conn: &Connection, last_id: u64, limit: u64)
    -> Result<Vec<(u64, R)>> {
    let cols = R::sql_columns(Some("files"));
    let sql = format!(
        "SELECT files.id AS pos, {cols}
         FROM files
         WHERE files.id > :last_id
           AND files.canonical_id = files.id
           AND files.sha1 IS NOT NULL
           AND files.ftype = 'file'
           AND (files.flags & :removed) = 0
           AND NOT EXISTS (
               SELECT 1 FROM out_tree AS r
               JOIN files AS rf ON r.file_id = rf.id
               WHERE rf.canonical_id = files.id
                 AND r.canonical_id = r.id
                 AND (r.flags & :terminal) = 0)
         ORDER BY files.id
         LIMIT :limit"
    );
    let mut stmt = conn.prepare(&sql).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":last_id": last_id as i64,
            ":limit": limit,
            ":removed": FileFlag::PayloadRemoved.mask_i64(),
            ":terminal": out_terminal() as i64,
        },
        |r: &rusqlite::Row<'_>| {
            let pos = r.get::<_, i64>("pos")? as u64;
            let record = R::from_row(r, Some("files"))?;
            Ok((pos, record))
        },
    ).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// The dedup-canonical file (`files.canonical_id = files.id`) whose cache
/// payload the just-materialized `out_tree` row served, iff that payload may now
/// be deleted: every self-canonical out-tree row serving the canonical has
/// settled on a terminal flag (`OUT_TERMINAL`) and the payload is not yet marked
/// removed (idempotent). `None` when the row is not a file, or a pending serving
/// row still needs the cache. The caller performs the actual `remove_file`.
pub fn deletable_canonical_payload(conn: &Connection, out_id: OutTreeId)
    -> Result<Option<FileId>> {
    let canonical_id: Option<i64> = conn
        .query_row(
            "SELECT f.canonical_id
             FROM out_tree AS o JOIN files AS f ON o.file_id = f.id
             WHERE o.id = :out_id
               AND f.canonical_id IS NOT NULL
               AND f.ftype = 'file'",
            named_params! { ":out_id": out_id.0 },
            |row| row.get(0),
        ).optional().to_panic()?;
    let Some(canonical_id) = canonical_id else { return Ok(None) };
    let removed: i64 = conn.query_row(
        "SELECT (flags & :removed) != 0 FROM files WHERE id = :canonical_id",
        named_params! {
            ":removed": FileFlag::PayloadRemoved.mask_i64(),
            ":canonical_id": canonical_id,
        },
        |row| row.get(0),
    ).to_panic()?;
    if removed != 0 {
        return Ok(None);
    }
    // Any pending self-canonical out row serving this canonical still needs its cache.
    let pending: i64 = conn.query_row(
        "SELECT COUNT(*) FROM out_tree AS r
         JOIN files AS rf ON r.file_id = rf.id
         WHERE rf.canonical_id = :canonical_id
           AND r.canonical_id = r.id
           AND (r.flags & :terminal) = 0",
        named_params! {
            ":canonical_id": canonical_id,
            ":terminal": out_terminal() as i64,
        },
        |row| row.get(0),
    ).to_panic()?;
    if pending > 0 {
        return Ok(None);
    }
    Ok(Some(FileId(canonical_id)))
}

/// For a whole batch of just-materialized canonical `out_tree` rows, the file
/// ids whose cache payloads may now be deleted (i.e. every self-canonical
/// serving row of that canonical is terminal and the payload is still present).
pub fn list_deletable_canonicals(conn: &Connection, out_ids: &[OutTreeId])
    -> Result<Vec<FileId>> {
    let mut result: Vec<FileId> = Vec::new();
    let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
    for out_id in out_ids {
        if let Some(canonical_id) = deletable_canonical_payload(conn, *out_id)? {
            if seen.insert(canonical_id.0) {
                result.push(canonical_id);
            }
        }
    }
    Ok(result)
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

/// Terminal flags that permanently settle an out_tree row's placement outcome:
/// placed, errored, or conflict-skipped. A row carrying any of these must not be
/// re-pulled on resume; nothing else marks it done.
fn out_terminal() -> u64 {
    OutTreeFlag::Placed.mask()
        | OutTreeFlag::ErrorWhilePlace.mask()
        | OutTreeFlag::Conflict.mask()
}

/// Next slice of still-pending canonical file rows, in `materialize_queue`
/// (size-DESC) order. `R` reads the dedup canonical (`f.canonical_id = c.id`).
/// Rejects rows already settled by a terminal flag (`OUT_TERMINAL`: Placed,
/// ErrorWhilePlace, or Conflict) from a previous session; the queue only
/// supplies the ordering. Returns `(queue_position, canonical_record,
/// out_tree_record)` triples.
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
           AND (o.flags & :terminal) = 0
         ORDER BY materialize_queue.id
         LIMIT :limit"
    );
    let mut stmt = conn.prepare(&sql).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":index": index as i64,
            ":terminal": out_terminal() as i64,
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

/// List unplaced hardlink rows — outputs aliasing an already-placed canonical.
/// Rows settled by a terminal flag (`OUT_TERMINAL`) from a previous session are
/// skipped, so a resumed run does not re-link them.
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
            AND (o.flags & :terminal) = 0
        ORDER BY o.id
        LIMIT :batch_size
    ")).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":last_id": last_id.0,
            ":terminal": out_terminal() as i64,
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

/// List unplaced "other"-type rows (symlink, fifo, device, socket). Rows settled
/// by a terminal flag (`OUT_TERMINAL`) from a previous session are skipped.
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
            AND (o.flags & :terminal) = 0
        ORDER BY o.id
        LIMIT :batch_size
    ")).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":last_id": last_id.0,
            ":terminal": out_terminal() as i64,
            ":batch_size": batch_size},
        |row| {
            let sr = R::from_row(row, Some("e"))?;
            let or = OutTreeRecord::from_sql(row, Some("o"))?;
            Ok((sr, or))
        },
    ).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// List all rows (which aren't directories) which remain to be linked into place.
/// `pending=true` lists the still-to-link set and relies on the caller marking
/// each row (Placed / ErrorWhilePlace) so a subsequent round excludes it.
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
        JOIN out_tree AS o ON ent.id = o.file_id
        WHERE ent.ftype NOT IN ('dir', 'unknown')
            {filter_placed}
            AND ent.flags & :moved != 0
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

    // Step 3: content-equal members whose (dev, inode) differs from every
    // elected row (separate files on the source, e.g. byte-identical copies)
    // are not a hardlink group — make each such out row its own canonical so
    // the materialize pass copies the payload to each path. The queue's source
    // resolves through `files.canonical_id`, so the (single) archived member is
    // still used as the copy source.
    let updated3 = conn.execute(
        "UPDATE out_tree SET canonical_id = id
         WHERE canonical_id IS NULL
           AND file_id IN (SELECT id FROM files WHERE ftype = 'file')",
        [],
    ).to_panic()?;

    Ok((updated + updated2 + updated3) as u64)
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

    // Step 3: content-equal members of this source with a distinct (dev, inode)
    // are not a hardlink group; materialize each independently (see
    // `mark_global_canonical`).
    let updated3 = conn.execute(
        "UPDATE out_tree SET canonical_id = id
         WHERE canonical_id IS NULL
            AND id IN (SELECT out_id FROM ref_out WHERE source_id = :source_id)
            AND file_id IN (SELECT id FROM files WHERE ftype = 'file')",
        named_params! { ":source_id": source_id },
    ).to_panic()?;

    Ok((updated + updated2 + updated3) as u64)
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
fn validate_materialize_result(m: &MaterializeResult) {
    // Universal invariants over every producible outcome (clean or errored): a
    // reflink/copy can only mark a placed row, and a removed entry implies a
    // conflict. Errored rows legitimately carry `{placed:false, conflict:false,
    // removed:false}`, so these are checked independent of `error`.
    assert!(!m.used_copy || m.placed, "INVARIANT FAILED: used_copy on a non-placed row");
    assert!(!m.removed || m.conflict, "INVARIANT FAILED: removed entry without a conflict");
}

/// Ingest a batch of MaterializeResults into the db within a single transaction
/// Apply an outcome's result flags (Placed/Conflict/Removed/IsHardlink/UsedRefLink).
/// The flag-constellation validator is only meaningful for clean (non-error)
/// outcomes; an errored row may legitimately report `{placed:false, conflict,
/// removed}` in any combination.
fn apply_result_flags(
    i_conn: &Connection,
    success: &MaterializeResult,
    is_hardlink: bool,
    set_reflink: bool)
    -> Result<()> {
    // Debug-only invariant, checked for every non-interrupted outcome (clean and
    // errored alike), since the flags are produced by the same `materialize_one`
    // logic.
    #[cfg(debug_assertions)]
    validate_materialize_result(success);
    set_out_tree_flag(i_conn, success.id, OutTreeFlag::Placed, success.placed)?;
    set_out_tree_flag(i_conn, success.id, OutTreeFlag::RemovedPrevious, success.removed)?;
    set_out_tree_flag(i_conn, success.id, OutTreeFlag::Conflict, success.conflict)?;

    if is_hardlink {
        set_out_tree_flag(i_conn, success.id, OutTreeFlag::IsHardlink, true)?;
    }

    if set_reflink {
        set_out_tree_flag(i_conn, success.id, OutTreeFlag::UsedRefLink, !success.used_copy)?;
    }

    Ok(())
}

pub fn ingest_materialize_results(
    conn: &mut Connection,
    results: &[MaterializeOutcome],
    is_hardlink: bool,
    set_reflink: bool)
    -> Result<()> {
    with_transaction(conn, |i_conn| {
        for outcome in results {
            match &outcome.error {
                // Force-abort mid-copy: leave the row pending (no flags).
                Some(Error::Interrupted) => (),
                Some(Error::FileStat(_)) => {
                    apply_result_flags(i_conn, &outcome.result, is_hardlink, set_reflink)?;
                    set_out_tree_flag(i_conn, outcome.result.id, OutTreeFlag::ErrorWhilePlace, true)?;
                }
                // Clean and errored rows share the same flags logic; validation —
                // inside `apply_result_flags` — runs for both, only skipping rows
                // that were force-aborted mid-copy (`Interrupted`).
                None => apply_result_flags(i_conn, &outcome.result, is_hardlink, set_reflink)?,
                Some(other) => {
                    panic!("INVARIANT FAILED: Only FileStat and Interrupted errors expected, got {}",
                           other)
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
// ---------------------------------------------------------------------------------------------------------------------
// TEST
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Real schema (same pattern as `db/place_prologue.rs::tests::conn`).
    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::initialize(&conn).unwrap();
        conn
    }

    fn insert_file(conn: &Connection, id: i64, abs: &str, dev: Option<i64>, inode: Option<i64>, canonical: i64, ftype: &str) {
        conn.execute(
            "INSERT INTO files (id, abs_path, abs_path_raw, ftype, ext, size, dev, inode, canonical_id) \
             VALUES (:id, :abs, :abs, :ftype, '', 0, :dev, :inode, :canonical)",
            named_params! {
                ":id": id, ":abs": abs, ":ftype": ftype,
                ":dev": dev, ":inode": inode, ":canonical": canonical,
            },
        ).unwrap();
    }

    fn insert_out(conn: &Connection, id: i64, abs: &str, file_id: i64) {
        conn.execute(
            "INSERT INTO out_tree (id, abs_path, file_id) VALUES (:id, :abs, :fid)",
            named_params! { ":id": id, ":abs": abs, ":fid": file_id },
        ).unwrap();
    }

    fn insert_source(conn: &Connection, id: i64, abs: &str) {
        conn.execute(
            "INSERT INTO source (id, source, abs_path, original_path, abs_path_raw, original_path_raw) \
             VALUES (:id, 'test', :abs, :abs, :abs, :abs)",
            named_params! { ":id": id, ":abs": abs },
        ).unwrap();
    }

    fn insert_ref_out(conn: &Connection, out_id: i64, source_id: i64) {
        conn.execute(
            "INSERT INTO ref_out (out_id, source_id) VALUES (:o, :s)",
            named_params! { ":o": out_id, ":s": source_id },
        ).unwrap();
    }

    fn canonical_ids(conn: &Connection) -> Vec<(i64, Option<i64>)> {
        let mut stmt = conn.prepare(&format!(
            "SELECT id, canonical_id FROM out_tree WHERE {} ORDER BY id",
            crate::db::common::id_gt_zero(),
        )).unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap()
    }

    #[test]
    fn mark_global_canonical_elects_one_per_hardlink_group() {
        let conn = conn();
        // Dedup content canonical 1 is self-linked; file 2 shares its (dev, inode)
        // hardlink group and points at it as content canonical.
        insert_file(&conn, 1, "/a/link_a", Some(1), Some(100), 1, "file");
        insert_file(&conn, 2, "/a/link_b", Some(1), Some(100), 1, "file");
        insert_out(&conn, 1, "/ed/a/link_a", 1);
        insert_out(&conn, 2, "/ed/a/link_b", 2);

        let updated = mark_global_canonical(&conn).unwrap();
        let ids = canonical_ids(&conn);
        // One out_tree row elected self-canonical; the other points at it.
        assert_eq!(ids, vec![(1, Some(1)), (2, Some(1))]);
        assert_eq!(updated, 2);
    }

    #[test]
    fn mark_source_canonical_only_groups_rows_of_that_source() {
        let conn = conn();
        insert_source(&conn, 1, "/a");
        insert_source(&conn, 2, "/b");
        insert_file(&conn, 1, "/a/x", Some(1), Some(100), 1, "file");
        insert_file(&conn, 2, "/b/y", Some(2), Some(200), 2, "file");
        insert_out(&conn, 1, "/ed/a/x", 1);
        insert_out(&conn, 2, "/ed/b/y", 2);
        insert_ref_out(&conn, 1, 1); // out 1 under source 1
        insert_ref_out(&conn, 2, 2); // out 2 under source 2

        let updated = mark_source_canonical(&conn, 1).unwrap();
        let ids = canonical_ids(&conn);
        // Only source-1's group elects; source-2's row is untouched.
        assert_eq!(ids, vec![(1, Some(1)), (2, None)]);
        assert_eq!(updated, 1);
    }

    #[test]
    fn mark_global_canonical_makes_separate_inode_copies_self_canonical() {
        let conn = conn();
        // Content canonical 1 collects byte-identical file 2, but file 2 is a
        // distinct inode (independent copy, not a hardlink) — it must be
        // self-canonical so the materialize pass places both paths.
        insert_file(&conn, 1, "/a/one", Some(1), Some(100), 1, "file");
        insert_file(&conn, 2, "/a/two", Some(1), Some(200), 1, "file");
        insert_out(&conn, 1, "/ed/a/one", 1);
        insert_out(&conn, 2, "/ed/a/two", 2);

        let updated = mark_global_canonical(&conn).unwrap();
        assert_eq!(canonical_ids(&conn), vec![(1, Some(1)), (2, Some(2))]);
        assert_eq!(updated, 2);
    }

    #[test]
    fn mark_source_canonical_makes_separate_inode_copies_self_canonical() {
        let conn = conn();
        insert_source(&conn, 1, "/a");
        insert_file(&conn, 1, "/a/x", Some(1), Some(100), 1, "file");
        insert_file(&conn, 2, "/a/y", Some(1), Some(200), 1, "file");
        insert_out(&conn, 1, "/ed/a/x", 1);
        insert_out(&conn, 2, "/ed/a/y", 2);
        insert_ref_out(&conn, 1, 1);
        insert_ref_out(&conn, 2, 1);

        let updated = mark_source_canonical(&conn, 1).unwrap();
        assert_eq!(canonical_ids(&conn), vec![(1, Some(1)), (2, Some(2))]);
        assert_eq!(updated, 2);
    }

    /// The `db/place.rs` doc comment: overlapping sources A=`/a/b` (covers b and
    /// c) and B=`/a/b/c` (covers c). The three links share one (dev, inode), but
    /// they only join into a single extraction-time group when the **covering**
    /// source is processed first; a nested source processed first claims c's
    /// subtree and the b-side link stays separate.
    #[test]
    fn mark_source_canonical_overlap_covering_source_first_groups_all() {
        let conn = conn();
        insert_source(&conn, 1, "/a/b");
        insert_source(&conn, 2, "/a/b/c");
        insert_file(&conn, 1, "/a/b/link_a", Some(1), Some(1), 1, "file");
        insert_file(&conn, 2, "/a/b/c/link_b", Some(1), Some(1), 2, "file");
        insert_file(&conn, 3, "/a/b/c/link_c", Some(1), Some(1), 3, "file");
        insert_out(&conn, 1, "/ed/a/b/link_a", 1);
        insert_out(&conn, 2, "/ed/a/b/c/link_b", 2);
        insert_out(&conn, 3, "/ed/a/b/c/link_c", 3);
        for (out, src) in [(1, 1), (2, 1), (3, 1), (2, 2), (3, 2)] {
            insert_ref_out(&conn, out, src);
        }
        mark_source_canonical(&conn, 1).unwrap();
        mark_source_canonical(&conn, 2).unwrap();
        assert_eq!(
            canonical_ids(&conn),
            vec![(1, Some(1)), (2, Some(1)), (3, Some(1))],
            "covering source first -> one group"
        );
    }

    #[test]
    fn mark_source_canonical_overlap_nested_source_first_splits() {
        let conn = conn();
        insert_source(&conn, 1, "/a/b/c");
        insert_source(&conn, 2, "/a/b");
        insert_file(&conn, 1, "/a/b/link_a", Some(1), Some(1), 1, "file");
        insert_file(&conn, 2, "/a/b/c/link_b", Some(1), Some(1), 2, "file");
        insert_file(&conn, 3, "/a/b/c/link_c", Some(1), Some(1), 3, "file");
        insert_out(&conn, 1, "/ed/a/b/link_a", 1);
        insert_out(&conn, 2, "/ed/a/b/c/link_b", 2);
        insert_out(&conn, 3, "/ed/a/b/c/link_c", 3);
        for (out, src) in [(1, 2), (2, 1), (3, 1), (2, 2), (3, 2)] {
            insert_ref_out(&conn, out, src);
        }
        mark_source_canonical(&conn, 1).unwrap();
        mark_source_canonical(&conn, 2).unwrap();
        assert_eq!(
            canonical_ids(&conn),
            vec![(1, Some(1)), (2, Some(2)), (3, Some(2))],
            "nested source first -> c group + separate b link"
        );
    }

    /// `--no-strict-separation` fanout: one out row served to both covering
    /// sources accumulates `ref_out` rows (with `INSERT OR IGNORE` dedupe).
    #[test]
    fn no_strict_separation_fanout_accumulates_ref_out() {
        let conn = conn();
        insert_source(&conn, 1, "/a");
        insert_source(&conn, 2, "/a/c");
        insert_file(&conn, 1, "/a/c/f.txt", Some(1), Some(1), 1, "file");
        insert_out(&conn, 1, "/ed/a/c/f.txt", 1);
        // Both covering sources fan out onto the single out row; the batched
        // inserter dedupes (INSERT OR IGNORE) the repeated pair.
        insert_ref_out_rows(
            &conn,
            &[(OutTreeId(1), 1), (OutTreeId(1), 2), (OutTreeId(1), 2)],
        ).unwrap();
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM ref_out", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM ref_out WHERE out_id = 1", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 2);
    }
}
