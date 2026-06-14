mod archive;
mod dedup;
mod extract;
mod hash;
mod inventory;
mod stage;

use crate::config::{Config, PipelinePhase, RuntimeState};
use crate::db::Database;
use crate::error::Result;
use crate::shutdown::Shutdown;

pub use extract::run as run_extract;

pub fn run_archive(config: Config, shutdown: Shutdown) -> Result<()> {
    let db = Database::open(&config.db_path())?;

    let mut state = match db.load_runtime_state()? {
        Some(state) if config.resume => state,
        _ => {
            let state = RuntimeState::new(config.jobs);
            db.save_runtime_state(&state)?;
            state
        }
    };

    while state.phase != PipelinePhase::Done {
        if shutdown.requested() {
            db.save_runtime_state(&state)?;
            tracing::warn!("shutdown requested; state saved");
            return Ok(());
        }

        run_phase(&state.phase, &config, &db, &shutdown)?;

        if let Some(next) = state.phase.next() {
            state.phase = next;
            db.save_runtime_state(&state)?;
        } else {
            break;
        }
    }

    Ok(())
}

fn run_phase(
    phase: &PipelinePhase,
    config: &Config,
    db: &Database,
    shutdown: &Shutdown,
) -> Result<()> {
    match phase {
        PipelinePhase::Inventory => inventory::run(config, db),
        PipelinePhase::Hash => hash::run(config, db),
        PipelinePhase::Dedup => dedup::run(config, db),
        PipelinePhase::Stage => stage::run(config, db),
        PipelinePhase::Archive => archive::run(config, db, shutdown),
        PipelinePhase::Done => Ok(()),
    }
}
