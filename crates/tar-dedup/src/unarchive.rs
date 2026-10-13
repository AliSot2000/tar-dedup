//! Unarchive (extract) pipeline: scan → rehash → placement_prologue → place → permissions → cleanup.

mod filter;
mod permissions;
mod place;
mod place_prologue;
mod rehash;
mod scan;

use std::path::Path;

use crate::common::cleanup::{self, CleanupMode};
use crate::common::start::{
    ProductPresence, StartAction, StartPolicy, WorkPresence, resolve_start,
};
use crate::config::{ExitAfterStage, ExtractConfig, ExtractPipelinePhase, ExtractRuntimeState};
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
    validate_exit_after_stage(&config)?;
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
    let mut state = load_extract_state(&db_path)?;

    let work = if db_path.is_file() && state.phase != ExtractPipelinePhase::Done {
        WorkPresence::Incomplete
    } else {
        WorkPresence::Absent
    };

    let mut pre_db_recorder = Recorder::speculative(
        !config.process.no_errors, Some(config.process.auto_flush_limit));
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
            tracing::info!("resuming extract from phase `{}`", state.phase.as_str());
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

        let completed = state.phase;

        match state.phase {
            ExtractPipelinePhase::ScanTar => {
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
                filter::run(&rt, true)?;
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

        // Cross-phase integrity battery after each completed phase (the Cleanup
        // arm breaks out before this point). See `plans/sanity-checks.md`.
        if let Some(edb) = db.as_ref() {
            let rt = ExtractRTArgs {
                config: &config,
                db: edb,
                shutdown: &shutdown,
                progress: &progress,
            };
            verify_after_phase(&rt, state.phase)?;
        }

        let Some(next) = state.phase.next() else {
            break;
        };
        state.phase = next;
        if let Some(ref edb) = db {
            edb.save_extract_runtime_state(&state)?;
        }

        // Exit after the configured stage: state is saved above, so a later
        // resume reprocesses from the next phase.
        if let Some(stop_after) = config
            .process
            .exit_after_stage
            .and_then(ExitAfterStage::stop_after_extract_phase) {
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

    bars.finish();
    drop(bars);

    if config.process.exit_after_stage == Some(ExitAfterStage::Cleanup) {
        tracing::info!("exit-after-stage `cleanup`: finished");
    }

    tracing::info!("extracted to {}", config.paths.extraction_root().display());
    Ok(())
}

/// Validate `--exit-after-stage` for the extract pipeline. Archive-only stages
/// have no extract counterpart; checked at run time because a resume override
/// bypasses `ExtractConfig::validate`.
fn validate_exit_after_stage(config: &ExtractConfig) -> Result<()> {
    if let Some(stage) = config.process.exit_after_stage {
        if !matches!(stage,
            ExitAfterStage::Inventory | ExitAfterStage::Hash
            | ExitAfterStage::Filter | ExitAfterStage::Cleanup) {
            return Err(Error::Config(format!(
                "--exit-after-stage {stage:?} does not exist on extract; valid stages: \
                 scan (inventory), hash (rehash), filter, cleanup"
            )));
        }
    }
    Ok(())
}

/// Run the cross-phase integrity battery (archive config-free subset first, then
/// the extract invariants) after `phase` completed. See `db/integrity.rs` and
/// `plans/sanity-checks.md`.
fn verify_after_phase(rt: &ExtractRTArgs, phase: ExtractPipelinePhase) -> Result<()> {
    let bar = rt.progress.begin_integrity(crate::db::extract_phase_check_count(rt.config, phase));
    let tick = || bar.inc(1);
    rt.db.integrity_check_after_extract(rt.config, phase, Some(&tick))
}

/// Map a phase to its global-bar anchor and swap in the matching phase bar.
fn enter_phase(progress: &ProgressBarSet, phase: ExtractPipelinePhase) {
    let (name, kind) = match phase {
        // TODO: Perhaps the thing should be COUNTER or COUNT depending on if we have --force or not
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn validate_exit_after_stage_accepts_extract_stages() {
        for stage in [
            ExitAfterStage::Inventory,
            ExitAfterStage::Hash,
            ExitAfterStage::Filter,
            ExitAfterStage::Cleanup,
        ] {
            let mut cfg = ExtractConfig::for_scan_test(PathBuf::new(), PathBuf::new(), PathBuf::new());
            cfg.process.exit_after_stage = Some(stage);
            assert!(validate_exit_after_stage(&cfg).is_ok(), "stage {stage:?}");
        }
    }

    #[test]
    fn validate_exit_after_stage_rejects_archive_only_stages() {
        for stage in [
            ExitAfterStage::Dedup,
            ExitAfterStage::Sparsify,
            ExitAfterStage::Stage,
            ExitAfterStage::Archive,
        ] {
            let mut cfg = ExtractConfig::for_scan_test(PathBuf::new(), PathBuf::new(), PathBuf::new());
            cfg.process.exit_after_stage = Some(stage);
            let err = validate_exit_after_stage(&cfg).unwrap_err();
            assert!(
                err.to_string().contains("does not exist on extract"),
                "stage {stage:?}: {err}",
            );
        }
    }
}
