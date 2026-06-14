use rusqlite::{params, Connection};

use crate::db::types::FileId;
use crate::error::Result;

pub fn set_canonical(conn: &Connection, file_id: FileId, canonical_id: FileId) -> Result<()> {
    conn.execute(
        "UPDATE files SET canonical_id = ?1, phase = 'deduped' WHERE id = ?2",
        params![canonical_id.0, file_id.0],
    )?;
    Ok(())
}

pub fn mark_self_canonical(conn: &Connection, file_id: FileId) -> Result<()> {
    conn.execute(
        "UPDATE files SET canonical_id = id, phase = 'deduped' WHERE id = ?1",
        params![file_id.0],
    )?;
    Ok(())
}

pub fn list_canonical_files(conn: &Connection) -> Result<Vec<FileId>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM files WHERE canonical_id = id AND phase = 'deduped' ORDER BY id",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0).map(FileId))?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}
