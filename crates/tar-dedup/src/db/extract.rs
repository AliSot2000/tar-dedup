use rusqlite::Connection;

use crate::config::ExtractRuntimeState;
use crate::db::common::SqlFileRow;
use crate::db::meta;
use crate::error::Result;

/// Cumulative + per-pass extract scan observations persisted in `meta`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExtractScanState {
    pub saw_manifest_db: bool,
    pub saw_any_members: bool,
    /// Set only when the tar entry iterator is exhausted.
    pub scan_complete: bool,
    /// Index of the last tar member that was fully processed; `None` while no member
    /// has been. A resumed pass restarts at the following index.
    pub last_member_index: Option<u64>,
    pub from_footer: bool,
    /// Cumulative `snapshot.sqlite` members ingested (persisted).
    pub snapshots_ingested: u32,
}

pub fn list_files_to_restore<R: SqlFileRow>(conn: &Connection) -> Result<Vec<R>> {
    let cols = R::sql_columns(None);
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols} FROM files WHERE phase = 'rehashed' ORDER BY id"
    ))?;
    let rows = stmt.query_map(
        [], |r| R::from_row(r, None))?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn load_extract_runtime_state(conn: &Connection) -> Result<Option<ExtractRuntimeState>> {
    let Some(phase) = meta::get_extract_phase(conn)? else {
        return Ok(None);
    };
    let snapshots_ingested = meta::get_extract_snapshots_ingested(conn)?.unwrap_or(0);
    Ok(Some(ExtractRuntimeState {
        phase,
        snapshots_ingested,
    }))
}

pub fn save_extract_runtime_state(conn: &mut Connection, state: &ExtractRuntimeState) -> Result<()> {
    meta::with_meta_txn(conn, |conn| {
        meta::set_extract_phase(conn, state.phase)?;
        meta::set_extract_snapshots_ingested(conn, state.snapshots_ingested)?;
        Ok(())
    })
}