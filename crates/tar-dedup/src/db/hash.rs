use rusqlite::{named_params, Connection};

use crate::db::types::FileId;
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
