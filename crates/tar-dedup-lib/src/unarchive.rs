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
    StartAction, StartPolicy, WorkPresence, resolve_start,
};
use crate::common::timed;
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


/// Resolve the extract work presence + runtime state from the work db.
///
/// Keyed purely on the work db: if it is absent, or carries no extract-phase
/// meta (a fresh bypass product's baked catalog is exactly that — not yet an
/// extract work-db), the work is `Absent`. A present db whose state is not
/// `Done` is `Incomplete`; a present db whose state is `Done` is `Done`.
fn resolve_extract_work(db_path: &Path) -> Result<(WorkPresence, ExtractRuntimeState)> {
    if db_path.is_file() {
        let db = Database::open(db_path)?;
        Ok(match db.load_extract_runtime_state()? {
            None => (WorkPresence::Absent, ExtractRuntimeState::new()),
            Some(ert) if ert.phase == ExtractPipelinePhase::Done => (WorkPresence::Done, ert),
            Some(ert) => (WorkPresence::Incomplete, ert),
        })
    } else {
        Ok((WorkPresence::Absent, ExtractRuntimeState::new()))
    }
}

pub fn run(outer_config: ExtractConfig, shutdown: Shutdown) -> Result<()> {
    let mut config = outer_config;
    validate_exit_after_stage(&config)?;
    let progress = ProgressBarSet::new(EXTRACT_MULTIPLIER);
    progress.set_table_size(10);
    register_mpb(progress.mp_handle());
    let mut bars = BarScope::new(&progress);

    if config.process.start_policy == StartPolicy::Fresh {
        let _ = cleanup::reset_workdir(&config);
    }
    std::fs::create_dir_all(&config.paths.work_dir)
        .map_err(|e| Error::io(&config.paths.work_dir, e))?;

    let (work, mut state) = resolve_extract_work(
        &config.paths.db_path()
    )?;

    let mut pre_db_recorder = Recorder::speculative(
        !config.process.no_errors, Some(config.process.auto_flush_limit));
    // Extract has no concrete "finished product" artifact to probe (a completed
    // run is signaled through `WorkPresence::Done`), so product = None.
    let action = resolve_start(config.process.start_policy, work, None)?;

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
            db = Some(Database::open(&config.paths.db_path())?);
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

        let phase_result: Result<()> = (|| {
            match state.phase {
                ExtractPipelinePhase::ScanTar => {
                    let (edb, scan_wrote) = timed(|| scan::run(&config, &shutdown,
                        &mut pre_db_recorder, &mut filter_buffer, &progress),
                        completed.as_str())?;
                    db = Some(edb);
                    // The scan phase reports whether it persisted the extract config
                    // (a bypass stray-file may have escalated keep_stage). Only write
                    // it here if the scan did not.
                    config_written |= scan_wrote;
                    if !config_written {
                        let ldb = db.as_ref().expect(OPT_DB_ERROR);
                        ldb.set_extract_config(&config)?;
                        config_written = true;
                    }
                    config = db
                        .as_ref()
                        .expect(OPT_DB_ERROR)
                        .get_extract_config()?
                        .expect("Config should have been set until now.");
                    progress.set_table_size(db
                        .as_ref()
                        .expect(OPT_DB_ERROR)
                        .count_entries()?);
                }
                ExtractPipelinePhase::Filter if let Some(ref edb) = db => {
                    let rt = ExtractRTArgs {
                        config: &config, db: edb, shutdown: &shutdown, progress: &progress
                    };
                    timed(|| filter::run(&rt, false), completed.as_str())?;
                }
                ExtractPipelinePhase::Rehash if let Some(ref edb) = db => {
                    let rt = ExtractRTArgs {
                        config: &config, db: edb, shutdown: &shutdown, progress: &progress
                    };
                    timed(|| rehash::run(&rt), completed.as_str())?;
                }
                ExtractPipelinePhase::PlacementPrologue if let Some(ref edb) = db => {
                    let rt = ExtractRTArgs {
                        config: &config, db: edb, shutdown: &shutdown, progress: &progress
                    };
                    timed(|| place_prologue::run(&rt), completed.as_str())?;
                }
                ExtractPipelinePhase::Place if let Some(ref edb) = db => {
                    let rt = ExtractRTArgs {
                        config: &config, db: edb, shutdown: &shutdown, progress: &progress
                    };
                    timed(|| place::run(&rt), completed.as_str())?;
                }
                ExtractPipelinePhase::Permissions if let Some(ref edb) = db => {
                    let rt = ExtractRTArgs {
                        config: &config, db: edb, shutdown: &shutdown, progress: &progress
                    };
                    timed(|| permissions::run(&rt), completed.as_str())?;
                }
                ExtractPipelinePhase::Cleanup if let Some(ref edb) = db => {
                    state.phase = ExtractPipelinePhase::Done;
                    edb.save_extract_runtime_state(&state)?;
                    timed(|| cleanup::cleanup_workdir(
                        &config, CleanupMode::Extract, config.bypass_archive),
                        completed.as_str())?;
                }
                other => panic!(
                    "INVARIANT ERROR: reached extract phase `{}` without a database",
                    other.as_str(),
                ),
            }
            Ok(())
        })();

        match phase_result {
            Ok(()) => {
                // Cleanup completed the run; exit the loop without the integrity
                // battery / transition machinery.
                if state.phase == ExtractPipelinePhase::Done {
                    break;
                }
            }
            Err(Error::Interrupted) => {
                if let Some(ref edb) = db {
                    edb.save_extract_runtime_state(&state)?;
                    // Only the resumable phases (scan, rehash) flush their
                    // mid-phase progress on interrupt; prologue / place /
                    // permissions must restart cleanly, so no checkpoint there.
                    if matches!(completed,
                        ExtractPipelinePhase::ScanTar | ExtractPipelinePhase::Rehash) {
                        edb.checkpoint()?;
                    }
                }
                return Ok(());
            }
            Err(e) => return Err(e),
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
            // Every phase transition is a durable boundary: the work-db file
            // must be self-contained (WAL TRUNCATE) once the next phase begins.
            edb.checkpoint()?;
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

    /// Round-trip: archive-bypass produces a product dir, then extract-bypass
/// validates it and materializes the output tree. The product dir doubles as the
/// extract work dir and must survive the run (it is the "archive", not a stage).
#[test]
fn bypass_archive_extract_round_trip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).expect("mk src");
    std::fs::write(src.join("hello.txt"), b"hello bypass\n").expect("write");
    std::fs::write(src.join("dup.txt"), b"hello bypass\n").expect("write dup");

    // 1. Archive side: -f <product dir> with bypass_archive.
    let mut acfg = crate::test_support::test_archive_config();
    acfg.paths.directory = dir.path().to_path_buf();
    acfg.paths.archive_path = dir.path().join("product");
    acfg.paths.output_dir = dir.path().to_path_buf();
    // Bypass rule: work dir IS the product dir.
    acfg.paths.work_dir = dir.path().join("product");
    acfg.inputs.input_dirs = vec![crate::config::PathSource {
        original_path: src.clone(),
        absolute_path: src,
    }];
    acfg.pipeline.write_archive_footer = false;
    acfg.pipeline.bypass_archive = true;
    acfg.process.start_policy = crate::common::start::StartPolicy::Create;
    // The product (work dir + baked catalog) must survive the archive cleanup.
    acfg.process.cleanup.keep_stage = crate::config::StageKeep::Always;
    acfg.process.cleanup.db_retention = crate::config::DbRetention::Always;
    crate::archive::run(acfg, Shutdown::detached()).expect("bypass archive");

    let product = dir.path().join("product");
    assert!(product.join("tar-dedup.sqlite").is_file(), "baked catalog present");

    // 2. Extract side: -f <same product> with bypass_archive, work-dir = product.
    let mut xcfg = ExtractConfig::default();
    xcfg.paths.archive_path = product.clone();
    xcfg.paths.directory = dir.path().to_path_buf();
    xcfg.paths.output_dir = dir.path().join("out");
    xcfg.paths.work_dir = product.clone();
    xcfg.bypass_archive = true;
    xcfg.rehash = true; // content verification runs over the product payloads in place
    xcfg.process.start_policy = crate::common::start::StartPolicy::Create;
    // The product dir (the "archive") must survive the extract cleanup too.
    xcfg.process.cleanup.keep_stage = crate::config::StageKeep::Always;
    xcfg.process.cleanup.db_retention = crate::config::DbRetention::Always;
    std::fs::create_dir_all(dir.path().join("out")).expect("mk out root");
    run(xcfg, Shutdown::detached()).expect("bypass extract");

    // The source dir was archived by absolute path, so members mirror the absolute
    // path under the extraction root (leading / stripped). Locate them by name.
    fn find(p: &Path, name: &str) -> Option<std::path::PathBuf> {
        for e in std::fs::read_dir(p).ok()?.flatten() {
            let ep = e.path();
            if ep.is_dir() {
                if let Some(f) = find(&ep, name) {
                    return Some(f);
                }
            } else if ep.file_name().map(|f| f == name).unwrap_or(false) {
                return Some(ep);
            }
        }
        None
    }
    let hello = find(&dir.path().join("out"), "hello.txt")
        .expect("hello.txt materialized under the output tree");
    let dup = find(&dir.path().join("out"), "dup.txt")
        .expect("dup.txt materialized under the output tree");
    assert_eq!(std::fs::read(&hello).expect("hello"), b"hello bypass\n");
    assert_eq!(std::fs::read(&dup).expect("dup"), b"hello bypass\n");
    // The product dir (the "archive") survives the extract run.
    assert!(product.join("tar-dedup.sqlite").is_file(), "product dir kept after extract");
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
    fn resolve_extract_work_covers_all_presence_states() {
        let dir = tempfile::tempdir().expect("tempdir");
        // No db -> Absent / fresh state.
        let (work, state) = resolve_extract_work(&dir.path().join("missing.sqlite"))
            .expect("absent defaults");
        assert_eq!(work, WorkPresence::Absent);
        assert_eq!(state.phase, ExtractPipelinePhase::ScanTar);
        assert_eq!(state.snapshots_ingested, 0);

        // Present db with no extract-phase meta (fresh bypass catalog) -> Absent.
        let db_path = dir.path().join("tar-dedup.sqlite");
        let db = Database::open(&db_path).expect("open");
        let (work, _) = resolve_extract_work(&db_path).expect("no-meta");
        assert_eq!(work, WorkPresence::Absent);

        // Present db with incomplete (non-Done) state -> Incomplete.
        db.save_extract_runtime_state(&ExtractRuntimeState {
            phase: ExtractPipelinePhase::Permissions,
            snapshots_ingested: 2,
        }).expect("save incomplete");
        let (work, state) = resolve_extract_work(&db_path).expect("incomplete");
        assert_eq!(work, WorkPresence::Incomplete);
        assert_eq!(state.phase, ExtractPipelinePhase::Permissions);

        // Present db with Done state -> Done.
        db.save_extract_runtime_state(&ExtractRuntimeState {
            phase: ExtractPipelinePhase::Done,
            snapshots_ingested: 3,
        }).expect("save done");
        let (work, _) = resolve_extract_work(&db_path).expect("done");
        assert_eq!(work, WorkPresence::Done);
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
