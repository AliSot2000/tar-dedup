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
use crate::config::{ArchiveConfig, ArchiveRuntimeState, ExitAfterStage, PipelinePhase};
use crate::db::Database;
use crate::error::{Error, Result};
use crate::progress::{ARCHIVE_MULTIPLIER, BarKind, BarScope, ProgressBarSet, register_mpb};
use crate::shutdown::Shutdown;

/// Runtime context threaded through the archive pipeline phases.
pub struct ArchiveRTArgs<'a> {
    pub config: &'a ArchiveConfig,
    pub db: &'a Database,
    pub shutdown: &'a Shutdown,
    pub progress: &'a ProgressBarSet,
}

pub fn run(config: ArchiveConfig, shutdown: Shutdown) -> Result<()> {
    // A finished product: a tar with a valid footer, or — in bypass mode — a
    // product directory holding a baked `tar-dedup.sqlite`.
    let product = if config.pipeline.bypass_archive {
        if config.paths.archive_path.join(crate::common::defaults::DB_NAME).is_file() {
            ProductPresence::Finished
        } else {
            ProductPresence::Absent
        }
    } else if archive_footer::has_valid_footer(&config.paths.archive_path) {
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

    let action = resolve_start(config.process.start_policy, work, Some(product))?;

    let db = Database::open(&db_path)?;
    let saved = db.load_runtime_state()?;

    let mut state = match action {
        StartAction::Resume => {
            let state = saved.expect("incomplete work checked above");
            tracing::info!("resuming from phase `{}`", state.phase.as_str());
            db.save_runtime_state(&state)?;
            state
        }
        StartAction::RunFresh => {
            let state = ArchiveRuntimeState::new();
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

    let progress = ProgressBarSet::new(ARCHIVE_MULTIPLIER);
    register_mpb(progress.mp_handle());
    let mut bars = BarScope::new(&progress);
    // Resume anchor: a work DB already holds the full table size.
    let entries = db.count_entries()?;
    let tbl_size = if entries > 0 { entries } else { 100 };
    progress.set_table_size(tbl_size);

    let rt = ArchiveRTArgs {
        config: &config,
        db: &db,
        shutdown: &shutdown,
        progress: &progress,
    };

    while state.phase != PipelinePhase::Done {
        shutdown.check_between_files()?;

        enter_phase(&progress, &state.phase);
        match run_phase(&state.phase, &rt) {
            Ok(()) => {
                verify_after_phase(&rt, state.phase)?;
                exit_phase(&progress, &state.phase, &db)?
            }
            Err(Error::Interrupted) => {
                db.save_runtime_state(&state)?;
                if shutdown.is_force() {
                    tracing::warn!(
                        "aborted during {}; in-flight progress discarded — rerun to resume",
                        state.phase.as_str()
                    );
                } else {
                    tracing::warn!(
                        "stopped during {}; completed work saved — rerun to resume",
                        state.phase.as_str()
                    );
                }
                return Ok(());
            }
            Err(e) => return Err(e),
        }

        let completed = state.phase;
        if let Some(next) = state.phase.next(config.filter.eager_filter) {
            state.phase = next;
            db.save_runtime_state(&state)?;
        } else {
            // Exit loop, if we have arrived at end of pipeline.
            break;
        }

        if let Some(stop_after) = config
            .process
            .exit_after_stage
            .and_then(|s| s.stop_after_phase()) {
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
        // PRECONDITION: Done should have left earlier. Here we should end up when we have
        debug_assert_ne!(completed, PipelinePhase::Done,
                         "Here, the Archive should have been created successfully");
    }
    // PRECONDITION: Successfully wrote to archive
    debug_assert_eq!(state.phase, PipelinePhase::Done,
                     "Here, the Archive should have been created successfully");

    bars.finish();
    drop(bars);

    drop(db);
    drop(lock);

    if config.pipeline.bypass_archive {
        tracing::info!(
            "bypass-archive product written to {}",
            config.paths.archive_path.display()
        );
    } else {
        tracing::info!(
            "archive written to {}",
            config.paths.archive_path.display()
        );
    }

    cleanup::cleanup_workdir(&config, CleanupMode::Archive, config.pipeline.bypass_archive)?;
    if config.process.cleanup.keep_stage.always() {
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

fn run_phase(phase: &PipelinePhase, rt: &ArchiveRTArgs) -> Result<()> {
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

/// Map a phase to its global-bar anchor and swap in the matching phase bar.
fn enter_phase(progress: &ProgressBarSet, phase: &PipelinePhase) {
    let (name, kind) = match phase {
        PipelinePhase::Inventory => ("inventoried sources:  ", BarKind::Counter),
        PipelinePhase::Hash => ("hash", BarKind::Count),
        PipelinePhase::Filter => ("filter", BarKind::Count),
        PipelinePhase::Dedup => ("dedup", BarKind::Count),
        PipelinePhase::Sparsify => ("sparsify", BarKind::Count),
        PipelinePhase::Stage => ("stage", BarKind::Counter),
        PipelinePhase::Archive => ("archive", BarKind::BytesRate),
        PipelinePhase::Done => ("done", BarKind::Counter),
    };
    progress.begin_phase(phase.index(), name, kind);
}

// ---------------------------------------------------------------------------------------------------------------------
// TEST
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CompressionFormat, CompressionSettings, PathSource};

    /// A full `ArchiveConfig` over a real source tree (dir, duplicate, symlink).
    fn full_archive_config(dir: &std::path::Path) -> ArchiveConfig {
        let src = dir.join("src");
        std::fs::create_dir_all(src.join("sub")).expect("create src/sub");
        std::fs::write(src.join("a.txt"), b"hello world").expect("write a");
        std::fs::write(src.join("sub/b.txt"), b"hello world").expect("write b dup");
        std::fs::write(src.join("sub/c.bin"), b"unique manhattan building\n").expect("write c");
        #[cfg(unix)]
        std::os::unix::fs::symlink("a.txt", src.join("link")).expect("symlink");

        let mut cfg = crate::test_support::test_archive_config();
        cfg.paths.directory = dir.to_path_buf();
        cfg.paths.archive_path = dir.join("test.astar");
        cfg.paths.output_dir = dir.to_path_buf();
        cfg.paths.work_dir = dir.join("astage");
        cfg.inputs.input_dirs = vec![PathSource {
            original_path: src.clone(),
            absolute_path: src,
        }];
        cfg.pipeline.dedup_mode = crate::cli::DedupMode::Regular;
        cfg.pipeline.write_archive_footer = true;
        cfg.process.start_policy = StartPolicy::Create;
        cfg.compression =
            CompressionSettings { format: CompressionFormat::None, ..cfg.compression.clone() };
        cfg
    }

    /// The finished archive is a plain tar with a valid seekable footer.
    fn assert_archive_complete(dir: &std::path::Path) {
        let archive = dir.join("test.astar");
        assert!(archive.is_file(), "archive written");
        assert!(
            crate::archive_footer::has_valid_footer(&archive),
            "archive carries a valid footer"
        );
        assert!(
            !dir.join("astage").exists(),
            "work dir cleaned after success"
        );
    }

    #[test]
    fn full_archive_run_end_to_end() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = full_archive_config(dir.path());
        run(cfg, Shutdown::detached()).expect("archive run");
        assert_archive_complete(dir.path());
    }

    #[test]
    fn archive_run_exit_after_stage_then_resume_to_done() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cfg = full_archive_config(dir.path());

        // Run half-way: finish inventory, save state, exit cleanly.
        cfg.process.exit_after_stage = Some(ExitAfterStage::Inventory);
        run(cfg.clone(), Shutdown::detached()).expect("inventory-only run");
        let archive = dir.path().join("test.astar");
        assert!(!archive.exists(), "no archive before the Archive phase");
        assert!(dir.path().join("astage").is_dir(), "work dir kept");

        // Resume from the saved state; the archived product completes.
        cfg.process.start_policy = StartPolicy::Resume;
        cfg.process.exit_after_stage = None;
        run(cfg, Shutdown::detached()).expect("resumed run");
        assert_archive_complete(dir.path());
    }

    #[test]
    fn archive_run_with_existing_finished_archive_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cfg = full_archive_config(dir.path());
        run(cfg.clone(), Shutdown::detached()).expect("first run");
        // A second Create run against a finished product must be rejected.
        let err = run(cfg, Shutdown::detached()).unwrap_err();
        assert!(
            err.to_string().contains("output already exists"),
            "{err}"
        );
    }

    #[test]
    fn produced_archive_footer_reads_back_its_catalog() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = full_archive_config(dir.path());
        run(cfg.clone(), Shutdown::detached()).expect("archive run");

        // The footer catalog installs as an openable work DB next to the archive.
        let dest = dir.path().join("footer.sqlite");
        assert!(
            crate::archive_footer::read_footer(&cfg.paths.archive_path, &dest).expect("read footer"),
            "footer present"
        );
        let db = Database::open(&dest).expect("open footer db");
        // 2 dirs (src, sub) + a.txt + sub/b.txt + sub/c.bin + link.
        assert_eq!(db.count_entries().expect("entries"), 6);
    }

    #[test]
    fn enter_phase_accepts_every_archive_phase() {
        let progress = ProgressBarSet::new(ARCHIVE_MULTIPLIER);
        for phase in [
            PipelinePhase::Inventory,
            PipelinePhase::Hash,
            PipelinePhase::Filter,
            PipelinePhase::Dedup,
            PipelinePhase::Sparsify,
            PipelinePhase::Stage,
            PipelinePhase::Archive,
            PipelinePhase::Done,
        ] {
            enter_phase(&progress, &phase);
        }
    }

    /// Bypass-archive config: `-f DIR` product with `bypass_archive` on.
    fn bypass_archive_config(dir: &std::path::Path) -> ArchiveConfig {
        let mut cfg = full_archive_config(dir);
        let product = dir.join("product");
        cfg.paths.archive_path = product.clone();
        // Bypass rule: work dir IS the product dir.
        cfg.paths.work_dir = product;
        cfg.pipeline.write_archive_footer = false;
        cfg.pipeline.bypass_archive = true;
        // The product (work dir + baked catalog) must survive cleanup.
        cfg.process.cleanup.keep_stage = crate::config::StageKeep::Always;
        cfg.process.cleanup.db_retention = crate::config::DbRetention::Always;
        cfg
    }

    #[test]
    fn bypass_archive_writes_dir_product_and_sets_meta() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = bypass_archive_config(dir.path());
        run(cfg, Shutdown::detached()).expect("bypass-archive run");

        let product = dir.path().join("product");
        assert!(product.is_dir(), "product dir exists");
        // A baked catalog sqlite sits beside the payload files.
        assert!(product.join("tar-dedup.sqlite").is_file(), "baked catalog present");
        let db = Database::open(&product.join("tar-dedup.sqlite")).expect("open product db");
        assert_eq!(db.get_bypass_archive().unwrap(), Some(true), "bypass meta set");
        assert_eq!(db.count_entries().unwrap(), 6, "6 entries (2 dirs + 3 files + symlink)");
        // The number of appended payload files must match the encoded-canonical
        // count, and each is materialized as a self-named content-id regular file.
        let encoded = db.count_encoded_payloads().unwrap();
        assert!(encoded >= 1, "at least one payload encoded");
        let mut payloads = 0;
        for entry in std::fs::read_dir(&product).expect("read product") {
            let entry = entry.expect("entry");
            let name = entry.file_name();
            // The catalog + its sqlite WAL/SHM companions aren't payloads.
            if name.to_string_lossy().starts_with("tar-dedup.sqlite") {
                continue;
            }
            assert!(entry.path().is_file(), "payload is a regular file");
            payloads += 1;
        }
        assert_eq!(payloads, encoded as usize, "payload files match encoded canonicals");
        drop(db);
        // Work dir cleaned after success.
        assert!(!dir.path().join("astage").exists(), "work dir cleaned");
    }
}

/// Run the cross-phase integrity battery against the archive invariants after
/// `phase` completed successfully. See `db/integrity.rs` and `plans/025-sanity-checks.md`.
fn verify_after_phase(rt: &ArchiveRTArgs, phase: PipelinePhase) -> Result<()> {
    let gate = crate::db::ArchiveGate {
        eager_filter: rt.config.filter.eager_filter,
        detect_hardlinks: !rt.config.indexing.no_hardlink_detection,
        sparsify: rt.config.sparse.sparsify,
        min_pages: rt.config.sparse.min_pages,
    };
    let bar = rt.progress.begin_integrity(crate::db::archive_phase_check_count(&gate, phase));
    let tick = || bar.inc(1);
    rt.db.integrity_check_after_archive(&gate, phase, Some(&tick))
}

/// Handle cleanup or any post operation actions. Importantly, these actions should only be run,
/// in case the phase that is being treated has finished successfully.
fn exit_phase(progress: &ProgressBarSet, phase: &PipelinePhase, db: &Database) -> Result<()> {
    match phase {
        PipelinePhase::Inventory => {progress.set_table_size(db.count_entries()?)},
        PipelinePhase::Hash => {},
        PipelinePhase::Filter => {},
        PipelinePhase::Dedup => {},
        PipelinePhase::Sparsify => {},
        PipelinePhase::Stage => {},
        PipelinePhase::Archive => {},
        PipelinePhase::Done => {},
    }
    Ok(())
}