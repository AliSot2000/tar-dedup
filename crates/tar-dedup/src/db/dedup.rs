use rusqlite::{named_params, Connection};

use crate::db::common::SqlFileRow;
use crate::db::flags::FileFlag;
use crate::db::types::{FileId, FilePhase, GroupKey};
use crate::error::Result;

pub fn set_canonical(conn: &Connection, file_id: FileId, canonical_id: FileId) -> Result<()> {
    conn.execute(
        "UPDATE files SET canonical_id = :canonical_id, phase = 'deduped' WHERE id = :id",
        named_params! {
            ":canonical_id": canonical_id.0,
            ":id": file_id.0,
        },
    )?;
    Ok(())
}

pub fn mark_self_canonical(conn: &Connection, file_id: FileId) -> Result<()> {
    conn.execute(
        "UPDATE files SET canonical_id = id, phase = 'deduped' WHERE id = :id",
        named_params! { ":id": file_id.0 },
    )?;
    Ok(())
}

/// Elect as active round canonical: self-pointer, stay in `Filtered`.
pub fn mark_active_canonical(conn: &Connection, file_id: FileId) -> Result<()> {
    conn.execute(
        "UPDATE files SET canonical_id = id, phase = 'filtered' WHERE id = :id",
        named_params! { ":id": file_id.0 },
    )?;
    Ok(())
}

pub fn promote_to_deduped(conn: &Connection, file_id: FileId) -> Result<()> {
    conn.execute(
        "UPDATE files SET phase = 'deduped' WHERE id = :id",
        named_params! { ":id": file_id.0 },
    )?;
    Ok(())
}

/// Non-regular / unknown types (and NULL ftype): nothing to byte-compare.
pub fn promote_non_file_filtered_to_deduped(conn: &Connection) -> Result<u64> {
    let n = conn.execute(
        "UPDATE files SET phase = 'deduped'
         WHERE phase = 'filtered' AND (ftype IS NULL OR ftype != 'file')",
        [],
    )?;
    Ok(n as u64)
}

/// Missing digest (e.g. unreadable at hash time): do not compare.
pub fn promote_null_sha1_filtered_to_deduped(conn: &Connection) -> Result<u64> {
    let n = conn.execute(
        "UPDATE files SET phase = 'deduped'
         WHERE phase = 'filtered' AND sha1 IS NULL",
        [],
    )?;
    Ok(n as u64)
}

/// Unique `(sha1, size)` content: no compare round.
pub fn promote_singleton_filtered_to_deduped(conn: &Connection) -> Result<u64> {
    let n = conn.execute(
        "UPDATE files SET phase = 'deduped'
         WHERE phase = 'filtered'
           AND sha1 IS NOT NULL
           AND (sha1, size) IN (
               SELECT sha1, size FROM files
               WHERE sha1 IS NOT NULL
               GROUP BY sha1, size
               HAVING COUNT(*) = 1
           )",
        [],
    )?;
    Ok(n as u64)
}

pub fn list_canonical_files(conn: &Connection, phase: FilePhase) -> Result<Vec<FileId>> {
    let phase_str = match phase {
        FilePhase::Deduped => "deduped",
        FilePhase::Sparsified => "sparsified",
        FilePhase::Staged => "staged",
        other => {
            return Err(crate::error::Error::Config(format!(
                "cannot list canonical files in phase {other:?}"
            )));
        }
    };
    let mut stmt = conn.prepare(
        "SELECT id FROM files WHERE canonical_id = id AND phase = :phase ORDER BY id",
    )?;
    let rows = stmt.query_map(named_params! { ":phase": phase_str }, |row| {
        row.get::<_, i64>("id").map(FileId)
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

fn parse_group_key_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<GroupKey> {
    let sha1_blob: Vec<u8> = row.get("sha1")?;
    let sha1: [u8; 20] = sha1_blob.try_into().map_err(|b: Vec<u8>| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("sha1 blob length {}, expected 20", b.len()),
            )),
        )
    })?;
    let size = row.get::<_, i64>("size")? as u64;
    Ok(GroupKey { sha1, size })
}

/// `(sha1, size)` buckets with more than one member and at least one still in `filtered`.
pub fn pending_duplicate_groups(conn: &Connection) -> Result<Vec<GroupKey>> {
    let mut stmt = conn.prepare(
        "SELECT sha1, size
         FROM files
         WHERE sha1 IS NOT NULL
         GROUP BY sha1, size
         HAVING COUNT(*) > 1
            AND SUM(CASE WHEN phase = 'filtered' THEN 1 ELSE 0 END) > 0",
    )?;

    let rows = stmt.query_map([], parse_group_key_row)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

/// Filtered members of one `(sha1, size)` group, ordered by id.
pub fn list_filtered_in_group<R: SqlFileRow>(
    conn: &Connection,
    sha1: &[u8; 20],
    size: u64,
) -> Result<Vec<R>> {
    let cols = R::sql_columns();
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols} FROM files
         WHERE sha1 = :sha1 AND size = :size AND phase = 'filtered'
         ORDER BY id"
    ))?;
    let rows = stmt.query_map(
        named_params! {
            ":sha1": sha1.as_slice(),
            ":size": size as i64,
        },
        R::from_row,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn clear_check_with_canonical_completed(
    conn: &Connection,
    sha1: &[u8; 20],
    size: u64,
) -> Result<()> {
    let bit = FileFlag::CheckWithCanonicalCompleted.mask_i64();
    conn.execute(
        "UPDATE files SET flags = flags & ~:bit
         WHERE sha1 = :sha1 AND size = :size AND (flags & :bit) != 0",
        named_params! {
            ":bit": bit,
            ":sha1": sha1.as_slice(),
            ":size": size as i64,
        },
    )?;
    Ok(())
}

/// Promote remaining pending members to `deduped`, leaving `canonical_id` NULL.
/// Returns how many rows were updated.
pub fn promote_errored_pending_to_deduped(
    conn: &Connection,
    sha1: &[u8; 20],
    size: u64,
) -> Result<u64> {
    let n = conn.execute(
        "UPDATE files SET phase = 'deduped'
         WHERE sha1 = :sha1 AND size = :size
           AND phase = 'filtered' AND canonical_id IS NULL",
        named_params! {
            ":sha1": sha1.as_slice(),
            ":size": size as i64,
        },
    )?;
    Ok(n as u64)
}

pub fn count_check_with_canonical_completed(conn: &Connection) -> Result<u64> {
    let bit = FileFlag::CheckWithCanonicalCompleted.mask_i64();
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files WHERE (flags & :bit) != 0",
        named_params! { ":bit": bit },
        |row| row.get(0),
    )?;
    Ok(n as u64)
}

/// Active round canonicals: `Filtered` with `canonical_id = id`.
pub fn count_active_canonicals(conn: &Connection, sha1: &[u8; 20], size: u64) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files
         WHERE sha1 = :sha1 AND size = :size
           AND phase = 'filtered'
           AND canonical_id = id",
        named_params! {
            ":sha1": sha1.as_slice(),
            ":size": size as i64,
        },
        |row| row.get(0),
    )?;
    Ok(n as u64)
}

/// Promote the single active canonical in the group to `Deduped`.
///
/// Panics unless exactly one row is updated.
pub fn promote_active_canonical_in_group(conn: &Connection, sha1: &[u8; 20], size: u64) {
    let n = conn
        .execute(
            "UPDATE files SET phase = 'deduped'
             WHERE sha1 = :sha1 AND size = :size
               AND phase = 'filtered'
               AND canonical_id = id",
            named_params! {
                ":sha1": sha1.as_slice(),
                ":size": size as i64,
            },
        )
        .expect("promote_active_canonical_in_group: UPDATE failed");
    assert_eq!(
        n, 1,
        "promote_active_canonical_in_group: expected exactly 1 active canonical, updated {n}"
    );
}

/// Rows that could become the next round's canonical (Filtered, no canonical, no error flag).
pub fn count_electable_pending(conn: &Connection, sha1: &[u8; 20], size: u64) -> Result<u64> {
    let error_bit = FileFlag::ErrorWhileDedup.mask_i64();
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files
         WHERE sha1 = :sha1 AND size = :size
           AND phase = 'filtered'
           AND canonical_id IS NULL
           AND (flags & :error_bit) = 0",
        named_params! {
            ":sha1": sha1.as_slice(),
            ":size": size as i64,
            ":error_bit": error_bit,
        },
        |row| row.get(0),
    )?;
    Ok(n as u64)
}
