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

// ---------------------------------------------------------------------------------------------------------------------
// TEST
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ArchiveRuntimeState, ExtractPipelinePhase, PipelinePhase};
    use crate::db::Database;

    #[test]
    fn archive_runtime_state_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::open(&dir.path().join("t.sqlite")).expect("open");
        assert!(db.load_archive_runtime_state().expect("load").is_none(), "empty db");

        let mut st = ArchiveRuntimeState::new();
        st.phase = PipelinePhase::Dedup;
        db.save_archive_runtime_state(&st).expect("save");
        let back = db.load_archive_runtime_state().expect("load").expect("saved");
        assert_eq!(back.phase, PipelinePhase::Dedup);
        // The timestamp survives the round-trip intact.
        assert_eq!(back.snapshot_taken_at, st.snapshot_taken_at);
        assert_eq!(db.get_archive_phase().expect("phase"), Some(PipelinePhase::Dedup));
    }

    #[test]
    fn extract_runtime_state_roundtrip_and_idempotent_init() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::open(&dir.path().join("t.sqlite")).expect("open");
        assert!(db.load_extract_runtime_state().expect("load").is_none());

        db.init_extract_runtime_state().expect("init");
        let init = db.load_extract_runtime_state().expect("load").expect("initialized");
        assert_eq!(init.phase, ExtractPipelinePhase::ScanTar);
        assert_eq!(init.snapshots_ingested, 0);

        let mut st = init;
        st.phase = ExtractPipelinePhase::Place;
        st.snapshots_ingested = 7;
        db.save_extract_runtime_state(&st).expect("save");
        let back = db.load_extract_runtime_state().expect("load").expect("saved");
        assert_eq!(back.phase, ExtractPipelinePhase::Place);
        assert_eq!(back.snapshots_ingested, 7);

        // init is a no-op once a state exists.
        db.init_extract_runtime_state().expect("re-init");
        let after = db.load_extract_runtime_state().expect("load").expect("after init");
        assert_eq!(after.phase, ExtractPipelinePhase::Place);
        assert_eq!(after.snapshots_ingested, 7);
    }

    #[test]
    fn archive_state_with_phase_but_no_snapshot_is_config_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = Database::open(&dir.path().join("t.sqlite")).expect("open");
        // Stage the meta phase directly, leaving the snapshot timestamp absent.
        db.with_transaction(|conn| {
            crate::db::meta::set_archive_phase(conn, PipelinePhase::Hash)?;
            Ok(())
        }).expect("set phase");
        assert!(matches!(db.load_archive_runtime_state(), Err(Error::Config(_))));
    }
}
