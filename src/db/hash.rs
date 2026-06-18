use rusqlite::{params, Connection};

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
        let ids_csv: String = row.get(2)?;
        let members = ids_csv
            .split(',')
            .filter_map(|s| s.parse::<i64>().ok().map(FileId))
            .collect();
        Ok(DuplicateGroup { members })
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}
