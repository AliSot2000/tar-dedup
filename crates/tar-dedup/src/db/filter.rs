use rusqlite::Connection;

use crate::error::Result;

/// Stub until a real filter stage exists: advance all hashed rows to filtered.
pub fn promote_hashed_to_filtered(conn: &Connection) -> Result<u64> {
    let n = conn.execute(
        "UPDATE files SET phase = 'filtered' WHERE phase = 'hashed'",
        [],
    )?;
    Ok(n as u64)
}
