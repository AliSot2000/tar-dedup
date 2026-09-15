//! Unarchive (extract) pipeline: scan → rehash → placement_prologue → place → permissions → cleanup.

mod filter;
mod permissions;
mod place;
mod place_prologue;
mod rehash;
mod scan;

pub use place_prologue::populate_out_tree; // INFO: Export for testing

use std::path::Path;

use crate::common::cleanup::{self, CleanupMode};
use crate::common::start::{
    ProductPresence, StartAction, StartPolicy, WorkPresence, resolve_start,
};
use crate::config::{ExtractConfig, ExtractPipelinePhase, ExtractRuntimeState};
use crate::db::{Database, Recorder};
use crate::error::{Error, Result};
use crate::shutdown::Shutdown;
use crate::unarchive::filter::{ParseFilterBuffer, ingest_filters};

pub fn run(config: ExtractConfig, shutdown: Shutdown) -> Result<()> {
    let product = ProductPresence::Absent;

    if config.process.start_policy == StartPolicy::Fresh {
        let _ = cleanup::reset_workdir(&config);
    }
    std::fs::create_dir_all(&config.paths.work_dir)
        .map_err(|e| Error::io(&config.paths.work_dir, e))?;

    let db_path = config.paths.db_path();
    if db_path.is_file() {
        let db = Database::open(&db_path)?;
        if config.scan.clear_archive_meta {
            db.clear_archive_meta()?;
        }
    }
    let mut state = load_extract_state(&db_path)?;

    let work = if db_path.is_file() && state.phase != ExtractPipelinePhase::Done {
        WorkPresence::Incomplete
    } else {
        WorkPresence::Absent
    };

    let mut pre_db_recorder = Recorder::speculative(!config.process.no_errors);
    let action = resolve_start(config.process.start_policy, work, product)?;
    let mut filter_buffer = match action {
        StartAction::RunFresh => {
            state = ExtractRuntimeState::new();
            Some(ingest_filters(&config, &mut pre_db_recorder)?)
        }
        StartAction::Resume => {
            tracing::error!("resuming extract from phase `{}`", state.phase.as_str());
            Some(ParseFilterBuffer::default())
        }
    };

    while state.phase != ExtractPipelinePhase::Done {
        shutdown.check_between_files()?;
        tracing::info!(phase = state.phase.as_str(), "unarchive phase");

        match state.phase {
            ExtractPipelinePhase::ScanTar => {
                tracing::error!("extract: scanning archive");
                let _db = scan::run(&config, &db_path, &shutdown,
                                    &mut pre_db_recorder, &mut filter_buffer)?;
            }
            ExtractPipelinePhase::Filter => {
                let db = Database::open(&db_path)?;
                filter::run(&db, &config, &shutdown)?;
            }
            ExtractPipelinePhase::Rehash => {
                let db = Database::open(&db_path)?;
                rehash::run(&config, &db, &shutdown)?;
            }
            ExtractPipelinePhase::PlacementPrologue => {
                let db = Database::open(&db_path)?;
                place_prologue::run(&config, &db, &shutdown)?;
            }
            ExtractPipelinePhase::Place => {
                let db = Database::open(&db_path)?;
                place::run(&config, &db, &shutdown)?;
            }
            ExtractPipelinePhase::Permissions => {
                let db = Database::open(&db_path)?;
                permissions::run(&config, &db, &shutdown)?;
            }
            ExtractPipelinePhase::Cleanup => {
                state.phase = ExtractPipelinePhase::Done;
                {
                    let db = Database::open(&db_path)?;
                    db.save_extract_runtime_state(&state)?;
                }
                cleanup::cleanup_workdir(&config, CleanupMode::Extract)?;
                if config.process.cleanup.keep_stage {
                    tracing::error!(
                        "keeping stage (--keep-stage): {}",
                        config.paths.work_dir.display()
                    );
                }
                break;
            }
            ExtractPipelinePhase::Done => break,
        }

        let Some(next) = state.phase.next() else {
            break;
        };
        state.phase = next;
        let db = Database::open(&db_path)?;
        db.save_extract_runtime_state(&state)?;
    }

    tracing::error!("extracted to {}", config.paths.extraction_root().display());
    Ok(())
}

fn load_extract_state(db_path: &Path) -> Result<ExtractRuntimeState> {
    if db_path.is_file() {
        let db = Database::open(db_path)?;
        Ok(db
            .load_extract_runtime_state()?
            .unwrap_or_else(ExtractRuntimeState::new))
    } else {
        Ok(ExtractRuntimeState::new())
    }
}
