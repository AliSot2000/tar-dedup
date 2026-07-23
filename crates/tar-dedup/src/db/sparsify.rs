use rusqlite::{named_params, Connection};

use crate::db::common::SqlFileRow;
use crate::db::flags::FileFlag;
use crate::db::types::FileId;
use crate::error::Result;

/// Advance every `deduped` row to `sparsified` (no HasSparse / canonical changes).
pub fn promote_deduped_to_sparsified(conn: &Connection) -> Result<u64> {
    let n = conn.execute(
        "UPDATE files SET phase = 'sparsified' WHERE phase = 'deduped'",
        [],
    )?;
    Ok(n as u64)
}

/// Candidate predicate fragments shared by list.
/// Bindings: `:min_pages`, `:has_sparse`.
const SPARSIFY_CANDIDATE_PRED: &str = "canonical_id = id
         AND ftype = 'file'
         AND sha1 IS NOT NULL
         AND sparse_count IS NOT NULL
         AND sparse_count >= :min_pages
         AND (flags & :has_sparse) = 0";

/// Promote Deduped rows that are **not** sparsify candidates (null-safe negation).
pub fn promote_non_sparsify_candidates_to_sparsified(
    conn: &Connection,
    min_pages: u64,
) -> Result<u64> {
    let has_sparse = FileFlag::HasSparse.mask_i64();
    let n = conn.execute(
        "UPDATE files SET phase = 'sparsified'
         WHERE phase = 'deduped'
           AND (
                canonical_id IS NULL OR canonical_id != id
             OR ftype IS NULL OR ftype != 'file'
             --technically implied by canonical_id IS NULL
             OR sha1 IS NULL 
             --sparse_count IS NULL implied by canonical_id IS NULL
             OR sparse_count IS NULL OR sparse_count < :min_pages 
             OR (flags & :has_sparse) != 0
           )",
        named_params! {
            ":min_pages": min_pages as i64,
            ":has_sparse": has_sparse,
        },
    )?;
    Ok(n as u64)
}

/// Deduped self-canonical regular files with enough empty pages and no HasSparse yet.
pub fn list_sparsify_candidates<R: SqlFileRow>(
    conn: &Connection,
    min_pages: u64,
) -> Result<Vec<R>> {
    let cols = R::sql_columns();
    let has_sparse = FileFlag::HasSparse.mask_i64();
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols} FROM files
         WHERE phase = 'deduped'
           AND ({SPARSIFY_CANDIDATE_PRED})
         ORDER BY id"
    ))?;
    let rows = stmt.query_map(
        named_params! {
            ":min_pages": min_pages as i64,
            ":has_sparse": has_sparse,
        },
        R::from_row,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn mark_sparsified_sparse(conn: &Connection, file_id: FileId) -> Result<()> {
    let bit = FileFlag::HasSparse.mask_i64();
    conn.execute(
        "UPDATE files SET phase = 'sparsified', flags = flags | :bit WHERE id = :id",
        named_params! {
            ":bit": bit,
            ":id": file_id.0,
        },
    )?;
    Ok(())
}

pub fn mark_sparsified_error(conn: &Connection, file_id: FileId) -> Result<()> {
    let bit = FileFlag::ErrorWhileSparsify.mask_i64();
    conn.execute(
        "UPDATE files SET phase = 'sparsified', flags = flags | :bit WHERE id = :id",
        named_params! {
            ":bit": bit,
            ":id": file_id.0,
        },
    )?;
    Ok(())
}
