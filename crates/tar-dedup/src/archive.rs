//! Archive (compress) pipeline: inventory → hash → dedup → stage → tar-writer.

mod dedup;
mod filter;
mod hash;
mod inventory;
mod sparsify;
mod stage;
mod tar_writer;

use std::fs::OpenOptions;

use fs4::fs_std::FileExt;

use crate::archive_footer;
use crate::common::cleanup::{self, CleanupMode};
use crate::common::start::{
    resolve_start, ProductPresence, StartAction, StartPolicy, WorkPresence,
};
use crate::config::{Config, PipelinePhase, RuntimeState};
use crate::db::Database;
use crate::error::{Error, Result};
use crate::shutdown::Shutdown;

pub fn run(config: Config, shutdown: Shutdown) -> Result<()> {
    let product = if archive_footer::has_valid_footer(&config.archive_path) {
        ProductPresence::Finished
    } else {
        ProductPresence::Absent
    };

    if config.start_policy == StartPolicy::Fresh {
        let _ = cleanup::reset_workdir(&config);
        std::fs::create_dir_all(&config.work_dir).map_err(|e| Error::io(&config.work_dir, e))?;
    }

    let lock = acquire_workdir_lock(&config)?;

    let db_path = config.db_path();
    let work = if db_path.is_file() {
        let probe = Database::open(&db_path)?;
        match probe.load_runtime_state()? {
            Some(state) if state.phase != PipelinePhase::Done => WorkPresence::Incomplete,
            _ => WorkPresence::Absent,
        }
    } else {
        WorkPresence::Absent
    };

    let action = resolve_start(config.start_policy, work, product)?;
    if action == StartAction::AlreadyDone {
        eprintln!(
            "archive already complete: {}",
            config.archive_path.display()
        );
        return Ok(());
    }

    let db = Database::open(&db_path)?;
    let saved = db.load_runtime_state()?;

    let mut state = match action {
        StartAction::Resume => {
            let mut state = saved.expect("incomplete work checked above");
            eprintln!("resuming from phase `{}`", state.phase.as_str());
            state.max_workers = config.jobs;
            db.save_runtime_state(&state)?;
            state
        }
        StartAction::RunFresh => {
            let state = RuntimeState::new(config.jobs);
            db.save_runtime_state(&state)?;
            state
        }
        StartAction::AlreadyDone => unreachable!(),
    };

    while state.phase != PipelinePhase::Done {
        shutdown.check_between_files()?;

        tracing::info!(phase = state.phase.as_str(), "archive phase");
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

    drop(db);
    drop(lock);

    eprintln!("archive written to {}", config.archive_path.display());

    cleanup::cleanup_workdir(&config, CleanupMode::Archive)?;
    if config.cleanup.keep_stage {
        eprintln!(
            "keeping stage (--keep-stage): {}",
            config.work_dir.display()
        );
    }
    if config.exit_after_stage == Some(crate::config::ExitAfterStage::Cleanup) {
        eprintln!("exit-after-stage `cleanup`: finished");
    }

    Ok(())
}

fn acquire_workdir_lock(config: &Config) -> Result<std::fs::File> {
    std::fs::create_dir_all(&config.work_dir).map_err(|e| Error::io(&config.work_dir, e))?;
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
        PipelinePhase::Sparsify => sparsify::run(config, db, shutdown),
        PipelinePhase::Stage => stage::run(config, db, shutdown),
        PipelinePhase::Archive => tar_writer::run(config, db, shutdown),
        PipelinePhase::Done => Ok(()),
    }
}
