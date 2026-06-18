mod archive;
mod dedup;
mod extract;
mod hash;
mod inventory;
mod stage;

use std::fs::OpenOptions;

use fs4::fs_std::FileExt;

use crate::config::{Config, PipelinePhase, RuntimeState};
use crate::db::Database;
use crate::error::{Error, Result};
use crate::shutdown::Shutdown;

pub use extract::run as run_extract;

pub fn run_archive(config: Config, shutdown: Shutdown) -> Result<()> {
    let _lock = acquire_workdir_lock(&config)?;

    let db = Database::open(&config.db_path())?;
    let saved = db.load_runtime_state()?;

    let mut state = if config.fresh {
        let state = RuntimeState::new(config.jobs);
        db.save_runtime_state(&state)?;
        state
    } else if should_resume(&config, &db, saved.as_ref()) {
        let mut state = saved.expect("checked above");
        if !config.resume {
            eprintln!("resuming from phase `{}`", state.phase.as_str());
        }
        state.max_workers = config.jobs;
        db.save_runtime_state(&state)?;
        state
    } else {
        let state = RuntimeState::new(config.jobs);
        db.save_runtime_state(&state)?;
        state
    };

    while state.phase != PipelinePhase::Done {
        shutdown.check_between_files()?;

        tracing::info!(phase = state.phase.as_str(), "pipeline phase");
        match run_phase(&state.phase, &config, &db, &shutdown) {
            Ok(()) => {}
            Err(Error::Interrupted) => {
                db.save_runtime_state(&state)?;
                if shutdown.is_force() {
                    eprintln!(
                        "aborted during {}; in-flight progress discarded — rerun to resume",
                        state.phase.as_str()
                    );
                } else {
                    eprintln!(
                        "stopped during {}; completed work saved — rerun to resume",
                        state.phase.as_str()
                    );
                }
                return Ok(());
            }
            Err(e) => return Err(e),
        }

        if let Some(next) = state.phase.next() {
            state.phase = next;
            db.save_runtime_state(&state)?;
        } else {
            break;
        }
    }

    Ok(())
}

fn should_resume(config: &Config, db: &Database, saved: Option<&RuntimeState>) -> bool {
    if config.resume {
        return saved.is_some();
    }
    match saved {
        Some(state) if state.phase != PipelinePhase::Done => true,
        _ => db.count_files().map(|n| n > 0).unwrap_or(false),
    }
}

fn acquire_workdir_lock(config: &Config) -> Result<std::fs::File> {
    let lock_path = config.work_dir.join(".lock");
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| crate::error::Error::io(&lock_path, e))?;
    lock.lock_exclusive()
        .map_err(|e| crate::error::Error::io(&lock_path, e))?;
    Ok(lock)
}

fn run_phase(
    phase: &PipelinePhase,
    config: &Config,
    db: &Database,
    shutdown: &Shutdown,
) -> Result<()> {
    match phase {
        PipelinePhase::Inventory => inventory::run(config, db, shutdown),
        PipelinePhase::Hash => hash::run(config, db, shutdown),
        PipelinePhase::Dedup => dedup::run(config, db, shutdown),
        PipelinePhase::Stage => stage::run(config, db, shutdown),
        PipelinePhase::Archive => archive::run(config, db, shutdown),
        PipelinePhase::Done => Ok(()),
    }
}
