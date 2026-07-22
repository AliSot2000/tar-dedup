use chrono::{DateTime, Utc};
use rusqlite::{named_params, Connection};

use crate::db::types::{FileId, FileRecord, FileType};
use crate::error::Result;

/// Columns needed to populate [`FileRecord`]. Schema names `xattr`/`acl`/`selinux`
/// map to Rust fields `xattrs`/`posix_acl`/`selinux_ctx`.
pub(crate) const FILES_SELECT: &str = "id, rel_path, size, sha1, mtime, atime, ctime, \
     uid, gid, mode, ftype, xattr, acl, selinux, canonical_id, tar_path, snapshot_archived";

pub fn get_file(conn: &Connection, file_id: FileId) -> Result<Option<FileRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {FILES_SELECT} FROM files WHERE id = :id"
    ))?;
    let mut rows = stmt.query(named_params! { ":id": file_id.0 })?;
    if let Some(row) = rows.next()? {
        return Ok(Some(map_file_record(row)?));
    }
    Ok(None)
}

pub(crate) fn map_file_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileRecord> {
    let sha1_blob: Option<Vec<u8>> = row.get("sha1")?;
    let sha1 = sha1_blob
        .and_then(|b| b.try_into().ok())
        .map(|arr: [u8; 20]| arr);

    Ok(FileRecord {
        id: FileId(row.get("id")?),
        rel_path: row.get::<_, String>("rel_path")?.into(),
        size: row.get::<_, i64>("size")? as u64,
        sha1,
        mtime: optional_rfc3339(row, "mtime")?,
        atime: optional_rfc3339(row, "atime")?,
        ctime: optional_rfc3339(row, "ctime")?,
        uid: row.get::<_, Option<i64>>("uid")?.map(|v| v as u32),
        gid: row.get::<_, Option<i64>>("gid")?.map(|v| v as u32),
        mode: row.get::<_, Option<i64>>("mode")?.map(|v| v as u32),
        ftype: optional_ftype(row, "ftype")?,
        xattrs: row.get("xattr")?,
        posix_acl: row.get("acl")?,
        selinux_ctx: row.get("selinux")?,
        canonical_id: row.get::<_, Option<i64>>("canonical_id")?.map(FileId),
        tar_path: row.get("tar_path")?,
        snapshot_archived: row.get::<_, i64>("snapshot_archived")? != 0,
    })
}

pub(crate) fn upsert_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (:key, :value)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        named_params! {
            ":key": key,
            ":value": value,
        },
    )?;
    Ok(())
}

fn optional_sha1(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<[u8; 20]>> {
    let sha1_blob: Option<Vec<u8>> = row.get("sha1")?;
    Ok(sha1_blob
        .and_then(|b| b.try_into().ok())
        .map(|arr: [u8; 20]| arr))
}

fn parse_phase(row: &rusqlite::Row<'_>) -> rusqlite::Result<FilePhase> {
    let raw: String = row.get("phase")?;
    FilePhase::parse(&raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                e.to_string(),
            )),
        )
    })
}

fn optional_rfc3339(
    row: &rusqlite::Row<'_>,
    column: &str,
) -> rusqlite::Result<Option<DateTime<Utc>>> {
    let raw: Option<String> = row.get(column)?;
    match raw {
        None => Ok(None),
        Some(s) => DateTime::parse_from_rfc3339(&s)
            .map(|dt| Some(dt.with_timezone(&Utc)))
            .map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            }),
    }
}

fn optional_ftype(
    row: &rusqlite::Row<'_>,
    column: &str,
) -> rusqlite::Result<Option<FileType>> {
    let raw: Option<String> = row.get(column)?;
    match raw {
        None => Ok(None),
        Some(s) => FileType::parse(&s).map(Some).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    e.to_string(),
                )),
            )
        }),
    }
}
