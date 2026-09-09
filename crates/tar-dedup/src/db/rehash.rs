use rusqlite::Connection;

/// Promote every `unarchived` row to `rehashed` without verifying payloads.
pub fn skip_rehash(conn: &Connection) -> crate::error::Result<u64> {
    // INFO: Technically, there should not be any file that is not 'unarchived' or 'rehashed'
    let n = conn.execute(
        "UPDATE files SET phase = 'rehashed' WHERE phase = 'unarchived'",
        [],
    )?;
    Ok(n as u64)
}
