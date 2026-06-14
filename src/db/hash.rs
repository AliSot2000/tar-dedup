use rusqlite::{params, Connection, OptionalExtension};

use crate::db::types::{DuplicateGroup, FileId};
use crate::error::Result;

pub fn set_sha1(conn: &Connection, file_id: FileId, digest: [u8; 20]) -> Result<()> {
    conn.execute(
        "UPDATE files SET sha1 = ?1, phase = 'hashed' WHERE id = ?2",
        params![digest.as_slice(), file_id.0],
    )?;
    Ok(())
}

pub fn duplicate_groups(conn: &Connection) -> Result<Vec<DuplicateGroup>> {
    let mut stmt = conn.prepare(
        "SELECT sha1, size, GROUP_CONCAT(id)
         FROM files
         WHERE sha1 IS NOT NULL
         GROUP BY sha1, size
         HAVING COUNT(*) > 1",
    )?;

    let rows = stmt.query_map([], |row| {
        let sha1_blob: Vec<u8> = row.get(0)?;
        let sha1: [u8; 20] = sha1_blob.try_into().map_err(|_| {
            rusqlite::Error::InvalidColumnType(0, "sha1".into(), rusqlite::types::Type::Blob)
        })?;
        let size = row.get::<_, i64>(1)? as u64;
        let ids_csv: String = row.get(2)?;
        let members = ids_csv
            .split(',')
            .filter_map(|s| s.parse::<i64>().ok().map(FileId))
            .collect();
        Ok(DuplicateGroup { sha1, size, members })
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn canonical_for(conn: &Connection, sha1: [u8; 20], size: u64) -> Result<Option<FileId>> {
    let id = conn
        .query_row(
            "SELECT id FROM files WHERE sha1 = ?1 AND size = ?2 ORDER BY id LIMIT 1",
            params![sha1.as_slice(), size as i64],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    Ok(id.map(FileId))
}
