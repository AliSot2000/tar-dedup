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
            progress.set_table_size(db
                .as_ref()
                .expect(OPT_DB_ERROR)
                .count_entries()?);
        }
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
                progress.set_table_size(db
                    .as_ref()
                    .expect(OPT_DB_ERROR)
                    .count_entries()?);
            }
            ExtractPipelinePhase::Filter if let Some(ref edb) = db => {
                let rt = ExtractRTArgs { config: &config, db: edb, shutdown: &shutdown, progress: &progress };
                filter::run(&rt, false)?;
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
        // arm breaks out before this point). See `plans/025-sanity-checks.md`.
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

    tracing::info!("extracted to {}", config.effective_extraction_root().display());
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
/// `plans/025-sanity-checks.md`.
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

// ---------------------------------------------------------------------------------------------------------------------
// TEST
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::build_footer_archive;
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

    /// Footer-archive config with rehash off (the shared catalog fabricates the
    /// digest); run() then still drives scan→filter→prologue→place→permissions→cleanup.
    fn footer_extract_config(dir: &Path) -> ExtractConfig {
        let (mut config, _member) = build_footer_archive(dir);
        config.rehash = false;
        // `build_path` short-circuits when a directory row equals the extraction
        // root, so the root must pre-exist (as a real extraction dir would).
        std::fs::create_dir_all(config.paths.extraction_root()).expect("create out root");
        config
    }

    fn assert_extracted(root: &Path) {
        assert_eq!(
            std::fs::read(root.join("canonical.txt")).expect("canonical"),
            b"data"
        );
        assert_eq!(
            std::fs::read(root.join("duplicate.txt")).expect("duplicate"),
            b"data",
            "dedup members materialize at their own paths"
        );
    }

    #[test]
    fn full_extract_run_end_to_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = footer_extract_config(dir.path());
        assert!(
            crate::archive_footer::has_valid_footer(&config.paths.archive_path),
            "fixture archive carries a footer"
        );
        run(config.clone(), Shutdown::detached()).expect("extract run");
        assert_extracted(config.paths.extraction_root());
        assert!(!config.paths.work_dir.exists(), "work dir cleaned after success");
    }

    #[test]
    fn extract_run_exit_after_stage_then_resume_to_done() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = footer_extract_config(dir.path());

        // Run through the scan only; state saved, output untouched.
        config.process.exit_after_stage = Some(ExitAfterStage::Inventory);
        run(config.clone(), Shutdown::detached()).expect("scan-only run");
        assert!(!config.paths.extraction_root().join("canonical.txt").exists());
        assert!(config.paths.work_dir.is_dir(), "work kept at stop");

        // Resume from the saved catalog; completes through cleanup.
        config.process.start_policy = StartPolicy::Resume;
        config.process.exit_after_stage = None;
        let root = config.paths.extraction_root().to_path_buf();
        let work = config.paths.work_dir.clone();
        run(config, Shutdown::detached()).expect("resumed run");
        assert_extracted(&root);
        assert!(!work.exists(), "work dir cleaned after resume");
    }

    #[test]
    fn load_extract_state_defaults_when_db_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = load_extract_state(&dir.path().join("missing.sqlite")).expect("default");
        assert_eq!(state.phase, ExtractPipelinePhase::ScanTar);
        assert_eq!(state.snapshots_ingested, 0);
    }

    #[test]
    fn enter_phase_accepts_every_extract_phase() {
        let progress = ProgressBarSet::new(EXTRACT_MULTIPLIER);
        for phase in [
            ExtractPipelinePhase::ScanTar,
            ExtractPipelinePhase::Filter,
            ExtractPipelinePhase::Rehash,
            ExtractPipelinePhase::PlacementPrologue,
            ExtractPipelinePhase::Place,
            ExtractPipelinePhase::Permissions,
            ExtractPipelinePhase::Cleanup,
            ExtractPipelinePhase::Done,
        ] {
            enter_phase(&progress, phase);
        }
    }
}
