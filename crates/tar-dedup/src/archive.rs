//! Archive (compress) pipeline: inventory → hash → filter → dedup → sparsify → stage → tar-writer.

mod dedup;
mod filter;
mod hash;
mod inventory;
mod sparsify;
mod stage;
mod tar_builder;

use std::fs::OpenOptions;

use fs4::fs_std::FileExt;

use crate::archive_footer;
use crate::common::cleanup::{self, CleanupMode};
use crate::common::start::{
    ProductPresence, StartAction, StartPolicy, WorkPresence, resolve_start,
};
use crate::config::{ArchiveConfig, ExitAfterStage, PipelinePhase, RuntimeState};
use crate::db::Database;
use crate::error::{Error, Result};
use crate::shutdown::Shutdown;

/// Runtime context threaded through the archive pipeline phases.
pub struct ArchiveRTArgs<'a> {
    pub config: &'a ArchiveConfig,
    pub db: &'a Database,
    pub shutdown: &'a Shutdown,
}

pub fn run(config: ArchiveConfig, shutdown: Shutdown) -> Result<()> {
    let product = if archive_footer::has_valid_footer(&config.paths.archive_path) {
        ProductPresence::Finished
    } else {
        ProductPresence::Absent
    };

    if config.process.start_policy == StartPolicy::Fresh {
        let _ = cleanup::reset_workdir(&config);
        std::fs::create_dir_all(&config.paths.work_dir)
            .map_err(|e| Error::io(&config.paths.work_dir, e))?;
    }

    let lock = acquire_workdir_lock(&config)?;

    let db_path = config.paths.db_path();
    let work = if db_path.is_file() {
        let probe = Database::open(&db_path)?;
        match probe.load_runtime_state()? {
            Some(state) if state.phase != PipelinePhase::Done => WorkPresence::Incomplete,
            _ => WorkPresence::Absent,
        }
    } else {
        WorkPresence::Absent
    };

    let action = resolve_start(config.process.start_policy, work, product)?;

    let db = Database::open(&db_path)?;
    let saved = db.load_runtime_state()?;

    let mut state = match action {
        StartAction::Resume => {
            let mut state = saved.expect("incomplete work checked above");
            tracing::info!("resuming from phase `{}`", state.phase.as_str());
            state.max_workers = config.process.jobs;
            db.save_runtime_state(&state)?;
            state
        }
        StartAction::RunFresh => {
            let state = RuntimeState::new(config.process.jobs);
            db.save_runtime_state(&state)?;
            db.set_archive_config(&config)?;
            filter::ingest_filters(&db, &config)?;
            if let Some(policy) = crate::common::perms::parse_owner_group_args(
                config.owner_policy.owner.as_deref(),
                config.owner_policy.owner_map.as_deref(),
                config.owner_policy.group.as_deref(),
                config.owner_policy.group_map.as_deref(),
            )? {
                db.set_archive_owner_policy(&policy)?;
            }
            // PRECONDITION: changes validated!
            if let Some(changes) = config.capture.mode.as_ref() {
                db.set_archive_mode_changes(changes)?;
            }
            // PRECONDITION: transform validated!
            if let Some(transform) = config.capture.transform.as_ref() {
                db.set_archive_transform(transform)?;
            }
            state
        }
    };

    while state.phase != PipelinePhase::Done {
        shutdown.check_between_files()?;

        let rt = ArchiveRTArgs {
            config: &config,
            db: &db,
            shutdown: &shutdown,
        };
        tracing::info!(phase = state.phase.as_str(), "archive phase");
        match run_phase(&state.phase, &rt) {
            Ok(()) => {}
            Err(Error::Interrupted) => {
                db.save_runtime_state(&state)?;
                if shutdown.is_force() {
                    tracing::error!(
                        "aborted during {}; in-flight progress discarded — rerun to resume",
                        state.phase.as_str()
                    );
                } else {
                    tracing::error!(
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

        if let Some(stop_after) = config
            .process
            .exit_after_stage
            .and_then(|s| s.stop_after_phase())
        {
            if completed == stop_after {
                tracing::info!(
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

    tracing::info!(
        "archive written to {}",
        config.paths.archive_path.display()
    );

    cleanup::cleanup_workdir(&config, CleanupMode::Archive)?;
    if config.process.cleanup.keep_stage {
        tracing::info!(
            "keeping stage (--keep-stage): {}",
            config.paths.work_dir.display()
        );
    }
    if config.process.exit_after_stage == Some(ExitAfterStage::Cleanup) {
        tracing::info!("exit-after-stage `cleanup`: finished");
    }

    Ok(())
}

fn acquire_workdir_lock(config: &ArchiveConfig) -> Result<std::fs::File> {
    std::fs::create_dir_all(&config.paths.work_dir)
        .map_err(|e| Error::io(&config.paths.work_dir, e))?;
    let lock_path = config.paths.work_dir.join(".lock");
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
    rt: &ArchiveRTArgs,
) -> Result<()> {
    match phase {
        PipelinePhase::Inventory => inventory::run(rt),
        PipelinePhase::Hash => hash::run(rt),
        PipelinePhase::Filter => filter::run(rt),
        PipelinePhase::Dedup => dedup::run(rt),
        PipelinePhase::Sparsify => sparsify::run(rt),
        PipelinePhase::Stage => stage::run(rt),
        PipelinePhase::Archive => tar_builder::run(rt),
        PipelinePhase::Done => Ok(()),
    }
}
