use rusqlite::{named_params, Connection};

use crate::db::types::{FileId, FilePhase};
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

pub fn list_canonical_files(conn: &Connection, phase: FilePhase) -> Result<Vec<FileId>> {
    let phase_str = match phase {
        FilePhase::Deduped => "deduped",
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
