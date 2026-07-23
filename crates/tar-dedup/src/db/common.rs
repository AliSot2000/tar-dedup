use std::path::Path;

use chrono::{DateTime, Utc};
use rusqlite::{named_params, Connection};

use crate::db::content_id::{content_id_from_digest, sparse_member_name};
use crate::db::flags::FileFlags;
use crate::db::types::{
    ContentId, ExclusionId, FileId, FilePhase, FileRecord, FileType, StrippedRecord,
};
use crate::error::Result;

/// Row type that can be SELECTed from `files` and mapped from a rusqlite row.
pub trait SqlFileRow: Sized {
    /// Comma-separated column list (no `SELECT` keyword).
    fn sql_columns() -> &'static str;
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self>;
    /// Content id only for self-canonical rows with a digest.
    fn content_id(&self) -> Option<ContentId>;
}

/// Assemble content_id iff canonical_id == id and file_type == File
fn content_id_if_canonical(
    id: FileId,
    canonical_id: Option<FileId>,
    sha1: Option<&[u8; 20]>,
    size: u64,
    rel_path: &Path,
    ftype: FileType,
) -> Option<ContentId> {
    if canonical_id != Some(id) {
        return None;
    }
    if ftype != FileType::File {
        return None;
    }
    let digest = sha1?;
    Some(content_id_from_digest(digest, size, id, rel_path))
}

impl FileRecord {
    /// Self-canonical regular file with digest → content id.
    pub fn content_id(&self) -> Option<ContentId> {
        content_id_if_canonical(
            self.id,
            self.canonical_id,
            self.sha1.as_ref(),
            self.size,
            &self.rel_path,
            self.ftype?,
        )
    }

    /// Tar/stage member basename (`{hash}.{size}.{fid}.ext`).
    pub fn tar_member_name(&self) -> Option<String> {
        self.content_id().map(|c| c.0)
    }

    /// Sparse rewrite basename (`sp.{content_id}`).
    pub fn sparse_member_name(&self) -> Option<String> {
        self.content_id().as_ref().map(sparse_member_name)
    }
}

impl StrippedRecord {
    /// Self-canonical regular file with digest → content id.
    pub fn content_id(&self) -> Option<ContentId> {
        content_id_if_canonical(
            self.id,
            self.canonical_id,
            self.sha1.as_ref(),
            self.size,
            &self.rel_path,
            self.ftype?,
        )
    }

    /// Tar/stage member basename (`{hash}.{size}.{fid}.ext`).
    pub fn tar_member_name(&self) -> Option<String> {
        self.content_id().map(|c| c.0)
    }

    /// Sparse rewrite basename (`sp.{content_id}`).
    pub fn sparse_member_name(&self) -> Option<String> {
        self.content_id().as_ref().map(sparse_member_name)
    }
}

impl SqlFileRow for FileRecord {
    fn sql_columns() -> &'static str {
        "id, rel_path, size, sha1, mtime, atime, ctime, \
         uid, gid, mode, ftype, xattr, acl, selinux, exclusion_id, canonical_id, flags, phase"
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(FileRecord {
            id: FileId(row.get("id")?),
            rel_path: row.get::<_, String>("rel_path")?.into(),
            size: row.get::<_, i64>("size")? as u64,
            sha1: optional_sha1(row)?,
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
            exclusion_id: row
                .get::<_, Option<i64>>("exclusion_id")?
                .map(ExclusionId),
            canonical_id: row.get::<_, Option<i64>>("canonical_id")?.map(FileId),
            flags: FileFlags::from_i64(row.get::<_, i64>("flags")?),
            phase: parse_phase(row)?,
        })
    }

    fn content_id(&self) -> Option<ContentId> {
        FileRecord::content_id(self)
    }
}

impl SqlFileRow for StrippedRecord {
    fn sql_columns() -> &'static str {
        "id, rel_path, size, sha1, mtime, atime, ctime, ftype, canonical_id, flags, phase"
    }

    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(StrippedRecord {
            id: FileId(row.get("id")?),
            rel_path: row.get::<_, String>("rel_path")?.into(),
            size: row.get::<_, i64>("size")? as u64,
            sha1: optional_sha1(row)?,
            mtime: optional_rfc3339(row, "mtime")?,
            atime: optional_rfc3339(row, "atime")?,
            ctime: optional_rfc3339(row, "ctime")?,
            ftype: optional_ftype(row, "ftype")?,
            canonical_id: row.get::<_, Option<i64>>("canonical_id")?.map(FileId),
            flags: FileFlags::from_i64(row.get::<_, i64>("flags")?),
            phase: parse_phase(row)?,
        })
    }

    fn content_id(&self) -> Option<ContentId> {
        StrippedRecord::content_id(self)
    }
}

pub fn get_file<R: SqlFileRow>(conn: &Connection, file_id: FileId) -> Result<Option<R>> {
    let cols = R::sql_columns();
    let mut stmt = conn.prepare(&format!("SELECT {cols} FROM files WHERE id = :id"))?;
    let mut rows = stmt.query(named_params! { ":id": file_id.0 })?;
    if let Some(row) = rows.next()? {
        return Ok(Some(R::from_row(row)?));
    }
    Ok(None)
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
