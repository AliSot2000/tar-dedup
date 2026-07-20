//! Unarchive (extract) pipeline: scan → rehash → place → permissions → cleanup.

mod cleanup;
mod permissions;
mod place;
mod rehash;
mod scan;

use std::path::Path;

use crate::config::{Config, ExtractPipelinePhase, ExtractRuntimeState};
use crate::db::Database;
use crate::error::Result;
use crate::shutdown::Shutdown;

pub fn run(config: Config, shutdown: Shutdown) -> Result<()> {
    if config.fresh {
        cleanup::reset_extract_work(&config)?;
    }

    let db_path = config.db_path();
    let mut state = load_extract_state(&db_path)?;

    if state.phase == ExtractPipelinePhase::Done {
        eprintln!(
            "extract already complete: {}",
            config.output_dir.display()
        );
        return Ok(());
    }

    while state.phase != ExtractPipelinePhase::Done {
        shutdown.check_between_files()?;
        tracing::info!(phase = state.phase.as_str(), "unarchive phase");

        match state.phase {
            ExtractPipelinePhase::ScanTar => {
                eprintln!("extract: scanning archive");
                let _db = scan::run(&config, &db_path, &shutdown)?;
            }
            ExtractPipelinePhase::Rehash => {
                let db = Database::open(&db_path)?;
                rehash::run(&config, &db, &shutdown)?;
            }
            ExtractPipelinePhase::Place => {
                let db = Database::open(&db_path)?;
                place::run(&config, &db, &shutdown)?;
                place::warn_catalog_uncertainty(&db)?;
            }
            ExtractPipelinePhase::Permissions => {
                let db = Database::open(&db_path)?;
                permissions::run(&config, &db, &shutdown)?;
            }
            ExtractPipelinePhase::Cleanup => {
                cleanup::run(&config)?;
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

    eprintln!("extracted to {}", config.output_dir.display());
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
