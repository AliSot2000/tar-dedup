use rusqlite::{named_params, Connection};

use crate::db::types::{DuplicateGroup, FileId};
use crate::error::Result;

pub fn update_file_inspection(
    conn: &Connection,
    file_id: FileId,
    digest: [u8; 20],
    sparse_count: u64,
) -> Result<()> {
    conn.execute(
        "UPDATE files SET sha1 = :sha1, sparse_count = :sparse_count, phase = 'hashed' WHERE id = :id",
        named_params! {
            ":sha1": digest.as_slice(),
            ":sparse_count": sparse_count as i64,
            ":id": file_id.0,
        },
    )?;
    Ok(())
}

pub fn duplicate_groups(conn: &Connection) -> Result<Vec<DuplicateGroup>> {
    let mut stmt = conn.prepare(
        "SELECT sha1, size, GROUP_CONCAT(id) AS ids
         FROM files
         WHERE sha1 IS NOT NULL
         GROUP BY sha1, size
         HAVING COUNT(*) > 1",
    )?;

    let rows = stmt.query_map([], |row| {
        let ids_csv: String = row.get("ids")?;
        let members = ids_csv
            .split(',')
            .filter_map(|s| s.parse::<i64>().ok().map(FileId))
            .collect();
        Ok(DuplicateGroup { members })
    })?;

    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}
