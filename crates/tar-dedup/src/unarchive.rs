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
use crate::progress::{BarKind, BarScope, EXTRACT_MULTIPLIER, ProgressBarSet, register_mpb};
use crate::shutdown::Shutdown;
use crate::unarchive::filter::{ParseFilterBuffer, ingest_filters};

const OPT_DB_ERROR: &str = "INVARIANT ERROR: Database expected to be present at this point";

/// Runtime context threaded through the extract pipeline phases.
/// The scan phase is the exception: it creates the catalog, so it takes its own
/// arguments (and may observe `db == None` internally).
pub struct ExtractRTArgs<'a> {
    pub config: &'a ExtractConfig,
    pub db: &'a Database,
    pub shutdown: &'a Shutdown,
    pub progress: &'a ProgressBarSet,
}

pub fn run(config: ExtractConfig, shutdown: Shutdown) -> Result<()> {
    let progress = ProgressBarSet::new(EXTRACT_MULTIPLIER);
    register_mpb(progress.mp_handle());
    let mut bars = BarScope::new(&progress);

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
    // TODO resume rework: seeding the db here is an interim measure; on resume the
    //  work DB already holds the catalog, so allowing any phase but scan to see None
    //  is purely defensive until resume is reworked.
    let mut db: Option<Database>;
    let mut filter_buffer: Option<ParseFilterBuffer>;
    // Fresh runs persist the config once the scan has installed the catalog DB.
    let mut config_written = false;
    match action {
        StartAction::RunFresh => {
            state = ExtractRuntimeState::new();
            db = None;
            filter_buffer = Some(ingest_filters(&config, &mut pre_db_recorder)?);
        }
        StartAction::Resume => {
            tracing::error!("resuming extract from phase `{}`", state.phase.as_str());
            db = Some(Database::open(&db_path)?);
            filter_buffer = Some(ParseFilterBuffer::default());
            config_written = true; // stored by the original run
        }
    }

    // Resume anchor: an existing work DB already holds the full catalog.
    if let Some(edb) = db.as_ref() {
        progress.set_table_size(edb.count_entries()?);
    }

    while state.phase != ExtractPipelinePhase::Done {
        shutdown.check_between_files()?;
        enter_phase(&progress, state.phase);
        tracing::info!(phase = state.phase.as_str(), "unarchive phase");

        match state.phase {
            ExtractPipelinePhase::ScanTar => {
                tracing::error!("extract: scanning archive");
                db = Some(scan::run(&config, &db_path, &shutdown,
                                    &mut pre_db_recorder, &mut filter_buffer, &progress)?);
                if !config_written {
                    let ldb = db.as_ref().expect(OPT_DB_ERROR);
                    ldb.set_extract_config(&config)?;
                    config_written = true;
                }
                let ldb = db.as_ref().expect(OPT_DB_ERROR);
                progress.set_table_size(ldb.count_entries()?);
            }
            ExtractPipelinePhase::Filter if let Some(ref edb) = db => {
                let rt = ExtractRTArgs { config: &config, db: edb, shutdown: &shutdown, progress: &progress };
                filter::run(&rt)?;
            }
            ExtractPipelinePhase::Rehash if let Some(ref edb) = db => {
                let rt = ExtractRTArgs { config: &config, db: edb, shutdown: &shutdown, progress: &progress };
                rehash::run(&rt)?;
            }
            ExtractPipelinePhase::PlacementPrologue if let Some(ref edb) = db => {
                let rt = ExtractRTArgs { config: &config, db: edb, shutdown: &shutdown, progress: &progress };
                place_prologue::run(&rt)?;
            }
            ExtractPipelinePhase::Place if let Some(ref edb) = db => {
                let rt = ExtractRTArgs { config: &config, db: edb, shutdown: &shutdown, progress: &progress };
                place::run(&rt)?;
            }
            ExtractPipelinePhase::Permissions if let Some(ref edb) = db => {
                let rt = ExtractRTArgs { config: &config, db: edb, shutdown: &shutdown, progress: &progress };
                permissions::run(&rt)?;
            }
            ExtractPipelinePhase::Cleanup if let Some(ref edb) = db => {
                state.phase = ExtractPipelinePhase::Done;
                edb.save_extract_runtime_state(&state)?;
                cleanup::cleanup_workdir(&config, CleanupMode::Extract)?;
                break;
            }
            other => panic!(
                "INVARIANT ERROR: reached extract phase `{}` without a database",
                other.as_str(),
            ),
        }

        let Some(next) = state.phase.next() else {
            break;
        };
        state.phase = next;
        if let Some(ref edb) = db {
            edb.save_extract_runtime_state(&state)?;
        }
    }

    bars.finish();
    drop(bars);

    tracing::error!("extracted to {}", config.paths.extraction_root().display());
    Ok(())
}

/// Map a phase to its global-bar anchor and swap in the matching phase bar.
fn enter_phase(progress: &ProgressBarSet, phase: ExtractPipelinePhase) {
    let (name, kind) = match phase {
        ExtractPipelinePhase::ScanTar => ("scan", BarKind::Counter),
        ExtractPipelinePhase::Filter => ("filter", BarKind::Counter),
        ExtractPipelinePhase::Rehash => ("rehash", BarKind::Count),
        ExtractPipelinePhase::PlacementPrologue => ("prologue", BarKind::Counter),
        ExtractPipelinePhase::Place => ("place", BarKind::Count),
        ExtractPipelinePhase::Permissions => ("permissions", BarKind::Counter),
        ExtractPipelinePhase::Cleanup => ("cleanup", BarKind::Counter),
        ExtractPipelinePhase::Done => ("done", BarKind::Counter),
    };
    progress.begin_phase(phase.index(), name, kind);
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
