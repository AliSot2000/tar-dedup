use rusqlite::Connection;

use crate::error::Result;

/// Advance every `deduped` row to `sparsified` (stub: no sparse rewrite yet).
pub fn promote_deduped_to_sparsified(conn: &Connection) -> Result<u64> {
    let n = conn.execute(
        "UPDATE files SET phase = 'sparsified' WHERE phase = 'deduped'",
        [],
    )?;
    Ok(n as u64)
}
