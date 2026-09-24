use rusqlite::{Connection, named_params};

use crate::db::common::{SqlFileRow, generate_archive_filter, mark_phase};
use crate::db::flags::{FileFlag, set_file_flag};
use crate::db::types::{FileId, FilePhase};
use crate::error::{Error, Result, ToPanic};

/// Result of a sparsify worker run for one file:
/// - `err: None` — the sparse rewrite succeeded (`HasSparse` + `sparsified`);
/// - `Some(Error::FileStat(_))` — per-file failure (`ErrorWhileSparsify` + `sparsified`);
/// - `Some(Error::Interrupted)` — aborted in-flight (row stays `deduped`).
pub struct SparseOutcome {
    pub id: FileId,
    pub modified: bool,
    pub err: Option<Error>,
}

/// Advance every `deduped` row to `sparsified` (no HasSparse / canonical changes).
pub fn promote_deduped_to_sparsified(conn: &Connection) -> Result<u64> {
    let n = conn.execute(
        "UPDATE files SET phase = 'sparsified' WHERE phase = 'deduped'",
        [],
    ).to_panic()?;
    Ok(n as u64)
}

/// Promote Deduped rows that are **not** sparsify candidates (null-safe negation).
pub fn promote_non_sparsify_candidates_to_sparsified(
    conn: &Connection,
    min_pages: u64,
) -> Result<u64> {
    let has_sparse = FileFlag::HasSparse.mask_i64();
    let filtered_rows = generate_archive_filter(None);
    let n = conn.execute(&format!(
        "UPDATE files SET phase = 'sparsified'
         WHERE phase = 'deduped'
           AND (
             canonical_id IS NULL
             OR canonical_id != id
             OR ftype != 'file'
             OR sha1 IS NULL               -- technically implied by canonical_id IS NULL
             OR sparse_count IS NULL       -- sparse_count IS NULL implied by canonical_id IS NULL
             OR sparse_count < :min_pages
             OR (flags & :has_sparse) != 0
             OR NOT ({filtered_rows})
         )"),
        named_params! {
            ":min_pages": min_pages as i64,
            ":has_sparse": has_sparse,
        },
    ).to_panic()?;
    Ok(n as u64)
}

/// Shared WHERE for the sparsify candidates — self-canonical deduped regular
/// files with enough empty pages, no sparse rewrite yet, archive-filter passed.
/// Params: `:min_pages`, `:has_sparse`.
fn sparsify_candidates_where() -> String {
    format!(
        "phase = 'deduped'
         AND canonical_id = id
         AND ftype = 'file'
         AND sparse_count >= :min_pages -- implies NOT NULL
         AND (flags & :has_sparse) = 0
         AND {}",
        generate_archive_filter(None)
    )
}

/// Create the `sparsify_queue` ordering table. Idempotent.
pub fn create_sparsify_queue(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS sparsify_queue (
             id      INTEGER PRIMARY KEY,
             file_id INTEGER NOT NULL UNIQUE REFERENCES files(id)
         )",
        [],
    ).to_panic()?;
    Ok(())
}

/// Populate `sparsify_queue` with the full, stable set of sparsify candidates in
/// `size DESC, id` order (position = `row_number()`). Idempotent: `INSERT OR
/// IGNORE` (via `UNIQUE(file_id)`) keeps rows from an earlier populate (e.g. a
/// resume) and adds only missing ones.
pub fn populate_sparsify_queue(conn: &Connection, min_pages: u64) -> Result<u64> {
    let sql = format!(
        "INSERT OR IGNORE INTO sparsify_queue (id, file_id)
         SELECT row_number() OVER (ORDER BY size DESC, id), id
         FROM files WHERE {}",
        sparsify_candidates_where()
    );
    let n = conn.execute(
        &sql,
        named_params! {
            ":min_pages": min_pages as i64,
            ":has_sparse": FileFlag::HasSparse.mask_i64(),
        },
    ).to_panic()?;
    Ok(n as u64)
}

/// Next slice of still-pending sparsify rows, in `sparsify_queue` (size-DESC)
/// order.
///
/// Joins the queue against `files`, returning only rows not yet sparsified
/// (`phase = 'deduped'`, no error flag), so a resume skips already-done files;
/// the queue only supplies the ordering. The returned `u64` is the queue
/// position of each row, letting the caller advance the read `index` across
/// slices. Same cursor contract as the hash queue; see `db/hash.rs`.
pub fn pull_pending_sparsify_rows<R: SqlFileRow>(
    conn: &Connection,
    index: u64,
    limit: u64,
) -> Result<Vec<(u64, R)>> {
    let cols = R::sql_columns(Some("files"));
    let sql = format!(
        "SELECT sparsify_queue.id AS pos, {cols}
         FROM files JOIN sparsify_queue ON sparsify_queue.file_id = files.id
         WHERE sparsify_queue.id > :index
           AND files.phase = 'deduped'
           AND (files.flags & :error_flag) = 0
         ORDER BY sparsify_queue.id
         LIMIT :limit"
    );
    let mut stmt = conn.prepare(&sql).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":index": index as i64,
            ":error_flag": FileFlag::ErrorWhileSparsify.mask_i64(),
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

/// Drop the `sparsify_queue` ordering table. Idempotent. Call on the
/// phase-success path only; leave it in place across interrupts/resumes so a
/// repopulate is a no-op for already-queued rows.
pub fn drop_sparsify_queue(conn: &Connection) -> Result<()> {
    conn.execute("DROP TABLE IF EXISTS sparsify_queue", []).to_panic()?;
    Ok(())
}

/// Count rows still awaiting a sparse rewrite — matches `pull_…` without the
/// limit. Errored rows already leave `deduped` at ingest, so the extra
/// `ErrorWhileSparsify` exclusion is belt-and-suspenders parity with `pull`.
pub fn count_pending_sparsify_candidates(conn: &Connection, min_pages: u64) -> Result<u64> {
    let sql = format!(
        "SELECT COUNT(*) AS count FROM files WHERE {} AND (flags & :error_flag) = 0",
        sparsify_candidates_where()
    );
    let count: i64 = conn.query_row(
        &sql,
        named_params! {
            ":min_pages": min_pages as i64,
            ":has_sparse": FileFlag::HasSparse.mask_i64(),
            ":error_flag": FileFlag::ErrorWhileSparsify.mask_i64(),
        },
        |row| row.get("count"),
    ).to_panic()?;
    Ok(count as u64)
}

/// Apply one batch of worker outcomes in a single transaction.
/// `None` err → `HasSparse` + `sparsified`; `FileStat` err → `ErrorWhileSparsify`
/// + `sparsified`; `Interrupted` → untouched (row stays `deduped` for the
/// resume). Returns the number of rows advanced to `sparsified`.
pub fn ingest_sparsify_outcome(
    conn: &mut Connection,
    results: &Vec<SparseOutcome>,
) -> Result<u64> {
    let tx = conn.transaction().to_panic()?;
    let mut resolved = 0u64;
    for outcome in results.iter() {
        if outcome.modified {
            assert_eq!(1, set_file_flag(&tx, outcome.id, FileFlag::Modified, true)?);
        }
        match &outcome.err {
            None => {
                assert_eq!(1, set_file_flag(&tx, outcome.id, FileFlag::HasSparse, true)?);
                mark_phase(&tx, outcome.id, FilePhase::Sparsified)?;
                resolved += 1;
            }
            Some(e) => match e {
                Error::FileStat(_) => {
                    assert_eq!(1, set_file_flag(
                        &tx, outcome.id, FileFlag::ErrorWhileSparsify, true)?);
                    mark_phase(&tx, outcome.id, FilePhase::Sparsified)?;
                    resolved += 1;
                }
                Error::Interrupted => (),
                other => panic!(
                    "Invariant Error. Only FileStatError and Interrupted expected. Got: {other}"
                ),
            },
        }
    }
    tx.commit().to_panic()?;
    Ok(resolved)
}