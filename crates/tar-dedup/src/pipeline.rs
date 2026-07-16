mod archive;
mod dedup;
mod extract;
mod hash;
mod inventory;
mod stage;
mod xattr;

use std::fs::{self, OpenOptions};

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

    if let Some(state) = saved.as_ref() {
        if state.phase == PipelinePhase::Done && !config.fresh {
            eprintln!(
                "archive already complete: {}",
                config.archive_path.display()
            );
            return Ok(());
        }
    }

    let mut state = if config.fresh {
        let state = RuntimeState::new(config.jobs);
        db.save_runtime_state(&state)?;
        state
    } else if should_resume(saved.as_ref()) {
        let mut state = saved.expect("checked above");
        eprintln!("resuming from phase `{}`", state.phase.as_str());
        state.max_workers = config.jobs;
        db.save_runtime_state(&state)?;
        state
    } else if config.resume {
        return Err(Error::Config(
            "no saved state to resume in work directory (omit --resume or use --fresh)".into(),
        ));
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

        let completed = state.phase;
        if let Some(next) = state.phase.next() {
            state.phase = next;
            db.save_runtime_state(&state)?;
        } else {
            break;
        }

        if let Some(stop_after) = config.exit_after_stage.and_then(|s| s.stop_after_phase()) {
            if completed == stop_after {
                eprintln!(
                    "exit-after-stage `{}`: finished `{}`, resume from `{}`",
                    stop_after.as_str(),
                    completed.as_str(),
                    state.phase.as_str()
                );
                return Ok(());
            }
        }
    }

    eprintln!(
        "archive written to {}",
        config.archive_path.display()
    );

    if config.exit_after_stage == Some(crate::config::ExitAfterStage::Cleanup) {
        if !config.keep_stage {
            cleanup_workdir(&config)?;
            eprintln!("exit-after-stage `cleanup`: work directory removed");
        } else {
            eprintln!(
                "exit-after-stage `cleanup`: keeping work dir (--keep-stage): {}",
                config.work_dir.display()
            );
        }
        return Ok(());
    }

    if !config.keep_stage {
        cleanup_workdir(&config)?;
    } else {
        eprintln!(
            "keeping work dir (--keep-stage): {}",
            config.work_dir.display()
        );
    }

    Ok(())
}

fn should_resume(saved: Option<&RuntimeState>) -> bool {
    match saved {
        Some(state) => state.phase != PipelinePhase::Done,
        None => false,
    }
}

fn cleanup_workdir(config: &Config) -> Result<()> {
    let stage = config.stage_dir();
    if stage.is_dir() {
        fs::remove_dir_all(&stage).map_err(|e| Error::io(&stage, e))?;
    }
    let db_path = config.db_path();
    if db_path.is_file() {
        fs::remove_file(&db_path).map_err(|e| Error::io(&db_path, e))?;
    }
    let lock = config.work_dir.join(".lock");
    if lock.is_file() {
        let _ = fs::remove_file(&lock);
    }
    Ok(())
}

fn acquire_workdir_lock(config: &Config) -> Result<std::fs::File> {
    let lock_path = config.work_dir.join(".lock");
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| Error::io(&lock_path, e))?;
    lock.lock_exclusive()
        .map_err(|e| Error::io(&lock_path, e))?;
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
