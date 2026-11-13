//! Unarchive (extract) pipeline: scan → rehash → placement_prologue → place → permissions → cleanup.

mod filter;
mod permissions;
mod place;
mod place_prologue;
mod rehash;
mod scan;

use std::path::Path;

use chrono::Utc;

use crate::common::cleanup::{self, CleanupMode};
use crate::common::defaults::{DB_NAME, SNAPSHOT_INIT_TAR_NAME};
use crate::common::files::copy_file_reflink;
use crate::common::start::{
    StartAction, StartPolicy, WorkPresence, resolve_start,
};
use crate::common::timed;
use crate::config::{ExitAfterStage, ExtractConfig, ExtractPipelinePhase, ExtractRuntimeState};
use crate::db::{Database, LedgerEvent, Recorder};
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
/// Establish the immutable bypass catalog and materialize a disposable extract
/// work DB from it.
///
/// A finished product carries `manifest.sqlite`, which is mandatory: extraction
/// never promotes `tar-dedup.sqlite` to a catalog. The only exception is an
/// explicit `--force` resume over an existing `Incomplete` work DB, so an
/// interrupted run can finish after the catalog was deleted. The work DB is
/// always a reflink/copy of the manifest, so extraction never mutates the
/// catalog.
fn ensure_bypass_catalog(config: &ExtractConfig, reset_work_db: bool) -> Result<()> {
    let work_db = config.paths.db_path();
    let manifest = config.paths.bypass_manifest_path();

    if !manifest.is_file() {
        let force_resume = !reset_work_db
            && config.process.start_policy == StartPolicy::Resume
            && config.process.force
            && matches!(resolve_extract_work(&work_db)?.0, WorkPresence::Incomplete);
        if force_resume {
            tracing::warn!(
                "bypass product {} has no {}; resuming from {} (--force)",
                config.paths.archive_path.display(),
                SNAPSHOT_INIT_TAR_NAME,
                work_db.display(),
            );
            return Ok(());
        }
        return Err(Error::Config(format!(
            "bypass product {} has no {}; re-run the archive{}",
            config.paths.archive_path.display(),
            SNAPSHOT_INIT_TAR_NAME,
            if config.process.start_policy == StartPolicy::Resume {
                " (or pass --force to resume from an existing work db)"
            } else {
                ""
            },
        )));
    }

    if reset_work_db || !work_db.is_file() {
        remove_work_db(&work_db)?;
        let tmp = work_db.with_file_name(format!("{DB_NAME}.tmp"));
        copy_file_reflink(&manifest, &tmp)?;
        std::fs::rename(&tmp, &work_db).map_err(|e| Error::io(&work_db, e))?;
    }
    Ok(())
}

/// Remove an extract work DB together with its SQLite companions.
fn remove_work_db(work_db: &Path) -> Result<()> {
    for path in [
        work_db.to_path_buf(),
        work_db.with_file_name(format!("{DB_NAME}-wal")),
        work_db.with_file_name(format!("{DB_NAME}-shm")),
        work_db.with_file_name(format!("{DB_NAME}.tmp")),
    ] {
        if path.is_file() {
            std::fs::remove_file(&path).map_err(|e| Error::io(&path, e))?;
        }
    }
    Ok(())
}

pub fn run(outer_config: ExtractConfig, shutdown: Shutdown) -> Result<()> {
    let mut config = outer_config;
    validate_exit_after_stage(&config)?;
    let progress = ProgressBarSet::new(EXTRACT_MULTIPLIER);
    progress.set_table_size(10);
    register_mpb(progress.mp_handle());
    let mut bars = BarScope::new(&progress);

    if config.process.start_policy == StartPolicy::Fresh && !config.bypass_archive {
        let _ = cleanup::reset_workdir(&config);
    }
    std::fs::create_dir_all(&config.paths.work_dir)
        .map_err(|e| Error::io(&config.paths.work_dir, e))?;

    // A bypass product's payloads/manifest must never be wiped by `--fresh`:
    // only the disposable work DB is reset, then re-materialized from the
    // immutable manifest.
    if config.bypass_archive {
        ensure_bypass_catalog(&config, config.process.start_policy == StartPolicy::Fresh)?;
    }

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
    match action {
        StartAction::RunFresh => {
            state = ExtractRuntimeState::new();
            db = None;
            filter_buffer = Some(ingest_filters(&config, &mut pre_db_recorder)?);
        }
        StartAction::Resume => {
            tracing::info!("resuming extract from phase `{}`", state.phase.as_str());
            let local_db = Database::open(&config.paths.db_path())?;
            filter_buffer = Some(ParseFilterBuffer::default());
            progress.set_table_size(local_db.count_entries()?);
            local_db.set_extract_config(&config)?; // INFO Storing overridden job / io-job values
            db = Some(local_db);
        }
    }

    while state.phase != ExtractPipelinePhase::Done {
        shutdown.check_between_files()?;
        enter_phase(&progress, state.phase);

        let completed = state.phase;
        let ledger_label = completed.ledger_label();
        let started_at = Utc::now();
        let ledger_started = if let Some(edb) = db.as_ref() {
            edb.record_phase_event(&ledger_label, LedgerEvent::Start, started_at);
            true
        } else {
            // The fresh footer-less scan installs the DB mid-phase; write a
            // back-dated Start once one exists.
            false
        };

        let phase_result: Result<()> = (|| {
            match state.phase {
                ExtractPipelinePhase::ScanTar => {
                    let (edb, scan_wrote) = timed(|| scan::run(&config, &shutdown,
                        &mut pre_db_recorder, &mut filter_buffer, &progress),
                        completed.as_str())?;
                    db = Some(edb);
                    if !scan_wrote {
                        let ldb = db.as_ref().expect(OPT_DB_ERROR);
                        ldb.set_extract_config(&config)?;
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
                if let Some(edb) = db.as_ref() {
                    if !ledger_started {
                        edb.record_phase_event(&ledger_label, LedgerEvent::Start, started_at);
                    }
                    edb.record_phase_event(&ledger_label, LedgerEvent::Stop, Utc::now());
                }
                // Cleanup completed the run; exit the loop without the integrity
                // battery / transition machinery.
                if state.phase == ExtractPipelinePhase::Done {
                    break;
                }
            }
            Err(Error::Interrupted) => {
                if let Some(ref edb) = db {
                    if !ledger_started {
                        edb.record_phase_event(&ledger_label, LedgerEvent::Start, started_at);
                    }
                    edb.record_phase_event(&ledger_label, LedgerEvent::Interrupt, Utc::now());
                    edb.save_extract_runtime_state(&state)?;
                    // Only the resumable phases (scan, rehash) flush their
                    // mid-phase progress on interrupt; prologue / place /
                    // permissions must restart cleanly, so no checkpoint there.
                    if matches!(completed, ExtractPipelinePhase::Rehash) {
                        edb.checkpoint()?;
                    } else if matches!(completed, ExtractPipelinePhase::ScanTar)
                        && !config.bypass_archive{
                        edb.checkpoint()?;
                    }
                }
                return Ok(());
            }
            Err(e) => {
                if let Some(ref edb) = db {
                    if !ledger_started {
                        edb.record_phase_event(&ledger_label, LedgerEvent::Start, started_at);
                    }
                    edb.record_phase_event(&ledger_label, LedgerEvent::Error, Utc::now());
                }
                return Err(e);
            }
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

    fn assert_ledger_start_stop(ledger: &[crate::db::TimeLedgerEntry], label: &str) {
        let entries: Vec<_> = ledger.iter().filter(|e| e.phase == label).collect();
        assert_eq!(entries.len(), 2, "{label}: {entries:?}");
        assert_eq!(entries[0].event, LedgerEvent::Start, "{label}");
        assert_eq!(entries[1].event, LedgerEvent::Stop, "{label}");
        assert!(entries[1].at >= entries[0].at, "{label}");
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
    assert!(product.join(SNAPSHOT_INIT_TAR_NAME).is_file(), "manifest present");
    assert!(!product.join(DB_NAME).exists(), "no work db before extract");

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
    // The immutable catalog survived extraction untouched; with retention
    // `Always` the disposable work DB is kept alongside it.
    let manifest = product.join(SNAPSHOT_INIT_TAR_NAME);
    assert!(manifest.is_file(), "manifest kept after extract");
    assert!(product.join(DB_NAME).is_file(), "work db kept (--db-retention always)");
    let mdb = Database::open(&manifest).expect("open manifest");
    assert_eq!(mdb.get_bypass_archive().unwrap(), Some(true), "manifest unchanged");
    assert!(
        mdb.load_extract_runtime_state().expect("manifest extract state").is_none(),
        "manifest was never mutated by extraction",
    );
    assert_eq!(mdb.count_errors(None).expect("manifest errors"), 0, "no stray-file errors");

    // The extract work DB inherits the archive ledger and appends extract events.
    let wdb = Database::open(&product.join(DB_NAME)).expect("open work db");
    let ledger = wdb.time_ledger().expect("time ledger");
    for label in [
        "archive::inventory",
        "archive::archive",
        "extract::scan_tar",
        "extract::place",
        "extract::cleanup",
    ] {
        assert_ledger_start_stop(&ledger, label);
    }
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

    fn bypass_catalog_config(dir: &Path) -> ExtractConfig {
        let product = dir.join("product");
        std::fs::create_dir_all(&product).expect("mk product");
        let mut cfg = ExtractConfig::default();
        cfg.paths.archive_path = product.clone();
        cfg.paths.work_dir = product.clone();
        cfg.paths.directory = dir.to_path_buf();
        cfg.paths.output_dir = dir.join("out");
        cfg.bypass_archive = true;
        cfg
    }

    /// Write a catalog as a finished archive left it: bypass meta + archive phase
    /// `Done`, no extract runtime state.
    fn write_finished_catalog(path: &Path) {
        let db = Database::open(path).expect("open catalog");
        db.set_bypass_archive(true).expect("bypass meta");
        let mut state = crate::config::ArchiveRuntimeState::new();
        state.phase = crate::config::PipelinePhase::Done;
        db.save_archive_runtime_state(&state).expect("archive done");
        db.checkpoint().expect("checkpoint");
    }

    #[test]
    fn ensure_bypass_catalog_requires_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cfg = bypass_catalog_config(dir.path());

        // Nothing at all.
        let err = ensure_bypass_catalog(&cfg, false).unwrap_err();
        assert!(err.to_string().contains("has no manifest.sqlite"), "{err}");

        // A finished work DB alone is never promoted to a manifest.
        write_finished_catalog(&cfg.paths.db_path());
        let err = ensure_bypass_catalog(&cfg, false).unwrap_err();
        assert!(err.to_string().contains("has no manifest.sqlite"), "{err}");
        assert!(cfg.paths.db_path().is_file(), "work db is not renamed");
        assert!(!cfg.paths.bypass_manifest_path().exists(), "no manifest fabricated");

        // `--force` only helps an explicit Resume over an Incomplete work DB.
        cfg.process.force = true;
        cfg.process.start_policy = StartPolicy::Resume;
        let err = ensure_bypass_catalog(&cfg, false).unwrap_err();
        assert!(err.to_string().contains("has no manifest.sqlite"), "{err}");

        let db = Database::open(&cfg.paths.db_path()).expect("open");
        db.save_extract_runtime_state(&ExtractRuntimeState {
            phase: ExtractPipelinePhase::Place,
            snapshots_ingested: 1,
        }).expect("dirty");
        db.checkpoint().expect("checkpoint");
        drop(db);

        ensure_bypass_catalog(&cfg, false).expect("forced resume");
        assert!(!cfg.paths.bypass_manifest_path().exists(), "no manifest fabricated");
        assert!(cfg.paths.db_path().is_file(), "work db untouched");
    }

    #[test]
    fn ensure_bypass_catalog_fresh_rebuilds_work_db_from_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = bypass_catalog_config(dir.path());
        let manifest = cfg.paths.work_dir.join(SNAPSHOT_INIT_TAR_NAME);
        write_finished_catalog(&manifest);

        // Materialize, dirty the work DB, then reset it via `--fresh`.
        ensure_bypass_catalog(&cfg, false).expect("materialize");
        let wdb = Database::open(&cfg.paths.db_path()).expect("open work");
        wdb.save_extract_runtime_state(&ExtractRuntimeState {
            phase: ExtractPipelinePhase::Place,
            snapshots_ingested: 42,
        }).expect("dirty");
        wdb.checkpoint().expect("checkpoint");
        drop(wdb);

        ensure_bypass_catalog(&cfg, true).expect("fresh reset");
        assert!(manifest.is_file(), "manifest untouched by fresh");
        let reset = Database::open(&cfg.paths.db_path()).expect("open reset work");
        assert!(reset.load_extract_runtime_state().expect("state").is_none(),
                "work db reset to the pristine manifest");
    }

    #[test]
    fn bypass_extract_done_cleanup_removes_work_db_but_keeps_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cfg = bypass_catalog_config(dir.path());
        let manifest = cfg.paths.work_dir.join(SNAPSHOT_INIT_TAR_NAME);
        write_finished_catalog(&manifest);

        cfg.process.cleanup.keep_stage = crate::config::StageKeep::Always;
        cfg.process.cleanup.db_retention = crate::config::DbRetention::OnError;

        ensure_bypass_catalog(&cfg, false).expect("materialize");
        assert!(cfg.paths.db_path().is_file(), "work db materialized");

        cleanup::cleanup_workdir(&cfg, CleanupMode::Extract, true).expect("cleanup");
        assert!(manifest.is_file(), "manifest survives cleanup");
        assert!(!cfg.paths.db_path().exists(), "clean done deletes the work db");
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
