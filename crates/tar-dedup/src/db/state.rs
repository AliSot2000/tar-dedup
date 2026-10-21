use crate::config::ExtractRuntimeState;
use crate::config::phases::ArchiveRuntimeState;
use crate::db::common::with_transaction;
use crate::db::meta;
use crate::error::{Error, Result, ToPanic};
use rusqlite::Connection;

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
    with_transaction(conn, |conn| {
        meta::set_extract_phase(conn, state.phase)?;
        meta::set_extract_snapshots_ingested(conn, state.snapshots_ingested)?;
        Ok(())
    })
}

pub fn load_archive_runtime_state(conn: &Connection) -> Result<Option<ArchiveRuntimeState>> {
    let Some(phase) = meta::get_archive_phase(conn)? else {
        return Ok(None);
    };
    let snapshot_taken_at = meta::get_archive_snapshot_taken_at(conn)?
        .ok_or_else(|| {
            Error::Config("missing archive_snapshot_taken_at in meta".into())
        })
        .to_panic()?;

    Ok(Some(ArchiveRuntimeState {
        snapshot_taken_at,
        phase,
    }))
}

pub fn save_archive_runtime_state(conn: &mut Connection, state: &ArchiveRuntimeState) -> Result<()> {
    with_transaction(conn, |conn| {
        meta::set_archive_phase(conn, state.phase)?;
        meta::set_archive_snapshot_taken_at(conn, state.snapshot_taken_at)?;
        Ok(())
    })
}

pub fn init_extract_runtime_state(conn: &mut Connection) -> Result<()> {
    if load_extract_runtime_state(conn)?.is_none() {
        save_extract_runtime_state(conn, &ExtractRuntimeState::new())?;
    }
    Ok(())
}
