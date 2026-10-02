use crate::db::SqlFileRow;
use crate::db::common::generate_archive_and_extract_filter;
use crate::db::common::mark_phase;
use crate::db::flags::{FileFlag, set_file_flag};
use crate::db::types::{FileId, FilePhase};
use crate::error::{FileStatError, Result, ToPanic};
use rusqlite::{Connection, named_params};

/// Promote every `extract_filtered` row to `rehashed` without verifying payloads.
pub fn skip_rehash(conn: &Connection) -> Result<u64> {
    // INFO: Technically, there should not be any file that is not 'extract_filtered' or 'rehashed'
    let n = conn.execute(
        "UPDATE files SET phase = 'rehashed' WHERE phase = 'extract_filtered'",
        [],
    ).to_panic()?;
    Ok(n as u64)
}

/// Shared WHERE selecting *all* rows the rehash phase verifies — the stable set,
/// independent of rehash progress (no phase predicate). Self-canonical regular
/// files that carry `FileExtracted`, passed the joint archive+extract filter, and
/// hold a digest to compare against. Used by both `count_files_to_rehash` and
/// `populate_rehash_queue`. Param: `:extracted`.
fn files_to_rehash_where() -> String {
    format!(
        "canonical_id = id
         AND ftype = 'file'
         AND sha1 IS NOT NULL
         AND (flags & :extracted) != 0
         AND {}",
        generate_archive_and_extract_filter(None)
    )
}

/// Count the **overall rehash workload** — elected rows regardless of phase.
/// Stable across sessions, so a resumed run's phase bar still reflects the full
/// workload (mirrors `db/hash.rs::count_all_hashable_files`).
pub fn count_files_to_rehash(conn: &Connection) -> Result<u64> {
    let sql = format!(
        "SELECT COUNT(*) AS count FROM files WHERE {}",
        files_to_rehash_where()
    );
    let count: i64 = conn.query_row(
        &sql,
        named_params! {
            ":extracted": FileFlag::FileExtracted.mask_i64(),
        },
        |row| row.get("count"),
    ).to_panic()?;
    Ok(count as u64)
}

/// Of `count_files_to_rehash`, how many are already `rehashed` — the resume
/// position on the phase bar (`pending` is the difference).
pub fn count_rehashed_files(conn: &Connection) -> Result<u64> {
    let sql = format!(
        "SELECT COUNT(*) AS count FROM files
         WHERE {} AND phase = 'rehashed'",
        files_to_rehash_where()
    );
    let count: i64 = conn.query_row(
        &sql,
        named_params! {
            ":extracted": FileFlag::FileExtracted.mask_i64(),
        },
        |row| row.get("count"),
    ).to_panic()?;
    Ok(count as u64)
}

/// Advance every `extract_filtered` row that is **not** elected (dupes,
/// non-files, filter-excluded, sha-less) straight to `rehashed`, so only elected
/// rows reach the queue. Null-safe negation (mirrors
/// `db/sparsify.rs::promote_non_sparsify_candidates_to_sparsified`).
pub fn promote_unrehashable_files(conn: &Connection) -> Result<u64> {
    let filtered = generate_archive_and_extract_filter(None);
    let n = conn.execute(&format!(
        "UPDATE files SET phase = 'rehashed'
         WHERE phase = 'extract_filtered'
           AND (
             canonical_id IS NULL
             OR canonical_id != id
             OR ftype != 'file'
             OR sha1 IS NULL
             OR (flags & :extracted) = 0
             OR NOT ({filtered})
         )"),
        named_params! {
            ":extracted": FileFlag::FileExtracted.mask_i64(),
        },
    ).to_panic()?;
    Ok(n as u64)
}

/// Create the `rehash_queue` ordering table. Idempotent.
pub fn create_rehash_queue(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS rehash_queue (
             id      INTEGER PRIMARY KEY,
             file_id INTEGER NOT NULL UNIQUE REFERENCES files(id)
         )",
        [],
    ).to_panic()?;
    Ok(())
}

/// Populate `rehash_queue` with the full, stable set of elected rows in
/// `size DESC, id` order (position = `row_number()`). Idempotent: `INSERT OR
/// IGNORE` (via `UNIQUE(file_id)`) keeps rows from an earlier populate (e.g. a
/// resume) and adds only missing ones.
pub fn populate_rehash_queue(conn: &Connection) -> Result<u64> {
    let sql = format!(
        "INSERT OR IGNORE INTO rehash_queue (id, file_id)
         SELECT row_number() OVER (ORDER BY size DESC, id), id
         FROM files WHERE {}",
        files_to_rehash_where()
    );
    let n = conn.execute(
        &sql,
        named_params! {
            ":extracted": FileFlag::FileExtracted.mask_i64(),
        },
    ).to_panic()?;
    Ok(n as u64)
}

/// Next slice of still-pending rehash rows, in `rehash_queue` (size-DESC) order.
///
/// Joins the queue against `files`, returning only rows not yet rehashed
/// (`phase = 'extract_filtered'`), so a resume skips already-done rows; the queue
/// only supplies the ordering. The returned `u64` is the queue position of each
/// row, letting the caller advance the read `index` across slices. Same cursor
/// contract as the hash queue; see `db/hash.rs`.
///
/// The predicate is deliberately **phase-only**: `ingest_rehash_outcome` sets any
/// sticky flag (`RehashMismatch` / `ErrorWhileRehashing`) *alongside*
/// `phase = 'rehashed'`, so an errored row is already excluded by its phase.
/// Revisit only if a retry loop is observed.
pub fn pull_pending_rehash_rows<R: SqlFileRow>(
    conn: &Connection,
    index: u64,
    limit: u64,
) -> Result<Vec<(u64, R)>> {
    let cols = R::sql_columns(Some("files"));
    let sql = format!(
        "SELECT rehash_queue.id AS pos, {cols}
         FROM files JOIN rehash_queue ON rehash_queue.file_id = files.id
         WHERE rehash_queue.id > :index
           AND files.phase = 'extract_filtered'
         ORDER BY rehash_queue.id
         LIMIT :limit"
    );
    let mut stmt = conn.prepare(&sql).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            ":index": index as i64,
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

/// Drop the `rehash_queue` ordering table. Idempotent. Call on the phase-success
/// path only; leave it in place across interrupts/resumes so a repopulate is a
/// no-op for already-queued rows.
pub fn drop_rehash_queue(conn: &Connection) -> Result<()> {
    conn.execute("DROP TABLE IF EXISTS rehash_queue", []).to_panic()?;
    Ok(())
}

/// Result of a rehash worker run for one file.
#[derive(Debug)]
pub enum RehashOutcome {
    /// Digest matches catalog `sha1` (or no payload to verify — duplicate row).
    Match(FileId),
    /// Digest differs from catalog `sha1`.
    Mismatch(FileId),
    /// Rehash failed; carries the file id, message and (best-effort) cache path.
    Errored(FileId, FileStatError),
}

/// Apply one batch of worker outcomes in a single transaction. Every variant
/// advances the row to `rehashed`; `Mismatch` additionally sets `RehashMismatch`
/// and `Error`/`Failed` set `ErrorWhileRehashing`. The persistent error-log rows
/// for `Error`/`Failed` are recorded caller-side (a `Recorder`), mirroring
/// `archive/hash.rs::record_hash_error`.
pub fn ingest_rehash_outcome(
    conn: &mut Connection,
    results: &Vec<RehashOutcome>)
    -> Result<u64> {
    let tx = conn.transaction().to_panic()?;
    for outcome in results.iter() {
        match outcome {
            RehashOutcome::Match(id) => {
                mark_phase(&tx, *id, FilePhase::Rehashed)?;
            }
            RehashOutcome::Mismatch(id) => {
                assert_eq!(1, set_file_flag(&tx, *id, FileFlag::RehashMismatch, true)?);
                mark_phase(&tx, *id, FilePhase::Rehashed)?;
            }
            RehashOutcome::Errored(id, _fse) => {
                assert_eq!(1, set_file_flag(&tx, *id, FileFlag::ErrorWhileRehashing, true)?);
                mark_phase(&tx, *id, FilePhase::Rehashed)?;
            }
        }
    }
    tx.commit().to_panic()?;
    Ok(0)
}
