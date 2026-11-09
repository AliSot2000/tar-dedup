//! Shared work-directory cleanup for archive (`.astage`) and extract (`.estage`).

use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::config::{DbRetention, WorkLayout, archive_stem};
use crate::error::{Error, Result};

pub use crate::config::CleanupMode;

/// Never touches archive or extraction trees (except relocating a kept DB).
pub fn cleanup_workdir(config: &impl WorkLayout, mode: CleanupMode) -> Result<()> {
    let db_path = config.paths().db_path();
    let keep = config.cleanup();
    let keep_db = resolve_keep_db(config, mode, &db_path)?;

    if keep_db && db_path.is_file() {
        let dest = retained_db_path(config, mode);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        fs::rename(&db_path, &dest).map_err(|e| Error::io(&dest, e))?;
        tracing::info!("kept database: {}", dest.display());
    } else if db_path.is_file() {
        fs::remove_file(&db_path).map_err(|e| Error::io(&db_path, e))?;
    }

    remove_temp_files(config);

    if !keep.keep_stage.always() {
        let work = &config.paths().work_dir;
        if work.is_dir() {
            match fs::remove_dir_all(work) {
                Ok(_) => (),
                Err(e) => tracing::warn!(
                    "failed to remove work dir {}: {}; leaving in place",
                    work.display(),
                    e,
                ),
            }
        }
    }

    Ok(())
}

/// Wipe work dir for `--fresh` (ignores keep flags).
pub fn reset_workdir(config: &impl WorkLayout) -> Result<()> {
    let db_path = config.paths().db_path();
    if db_path.is_file() {
        fs::remove_file(&db_path).map_err(|e| Error::io(&db_path, e))?;
    }
    remove_temp_files(config);
    let work = &config.paths().work_dir;
    if work.is_dir() {
        fs::remove_dir_all(work).map_err(|e| Error::io(work, e))?;
    }
    Ok(())
}

fn remove_temp_files(config: &impl WorkLayout) {
    for name in [".snapshot-ingest.tmp", ".snapshot-for-tar.sqlite", ".lock"] {
        let p = config.paths().work_dir.join(name);
        if p.is_file() {
            let _ = fs::remove_file(&p);
        }
    }
}

fn resolve_keep_db(config: &impl WorkLayout, mode: CleanupMode, db_path: &Path)
    -> Result<bool> {
    match config.cleanup().db_retention {
        DbRetention::Always => Ok(true),
        DbRetention::Never => Ok(false),
        // On-error retention counts the recorded errors before the DB is moved
        // or deleted. `--no-errors` records nothing, so the count is 0 and the
        // DB is dropped (mirrors the validation-time warn).
        DbRetention::OnError => {
            if !db_path.is_file() {
                return Ok(false);
            }
            let db = crate::db::Database::open(db_path)?;
            Ok(db.count_pipeline_errors(mode)? > 0)
        }
    }
}

fn retained_db_path(config: &impl WorkLayout, mode: CleanupMode) -> PathBuf {
    let stem = archive_stem(&config.paths().archive_path);
    let ts = Utc::now().to_rfc3339().replace(':', "-");
    let name = format!("{ts}_{stem}.sqlite");
    config.kept_db_parent(mode).join(name)
}

// ---------------------------------------------------------------------------------------------------------------------
// TEST
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{ExtractWorld, TestWorld};

    const TEMP_NAMES: [&str; 3] = [".snapshot-ingest.tmp", ".snapshot-for-tar.sqlite", ".lock"];

    /// True when the effective uid is root (the warn-and-keep remove-failure
    /// paths cannot be forced as root, since write permissions are bypassed).
    fn running_as_root() -> bool {
        nix::unistd::geteuid().is_root()
    }

    fn seed_work_dir_files(world_config: &impl WorkLayout) {
        for name in TEMP_NAMES {
            std::fs::write(world_config.paths().work_dir.join(name), b"x").expect("write temp");
        }
        // The DB itself: opening a Database at db_path() creates the file.
        assert!(world_config.paths().db_path().is_file(), "db present");
    }

    /// Every relation that `cleanup_workdir` may see is exercised via the shared
    /// archive world; extract differs only in `kept_db_parent`.
    fn archive_world(keep_db: bool, keep_stage: bool, materialize_db: bool) -> TestWorld {
        let world = TestWorld::tuned(|c| {
            c.process.cleanup.db_retention = if keep_db {
                DbRetention::Always
            } else {
                DbRetention::Never
            };
            c.process.cleanup.keep_stage = keep_stage;
        });
        if materialize_db {
            seed_work_dir_files(&world.config);
        }
        world
    }

    fn kept_dbs_in(parent: &std::path::Path, stem: &str) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(parent).expect("read parent") {
            let p = entry.expect("entry").path();
            if p.extension().map(|e| e == "sqlite").unwrap_or(false) && p.file_name().unwrap_or_default().to_string_lossy().ends_with(&format!("_{stem}.sqlite")) {
                out.push(p);
            }
        }
        out
    }

    fn assert_kept_only(db_before: Option<PathBuf>, root: &std::path::Path, stem: &str) {
        // The original db_path no longer holds the db; exactly one kept copy exists.
        if let Some(db_before) = db_before {
            assert!(!db_before.is_file(), "original db must be gone");
        }
        let kept = kept_dbs_in(root, stem);
        assert_eq!(kept.len(), 1, "kept dbs under {root:?}: {kept:?}");
        assert!(kept[0].is_file(), "kept db exists at {}", kept[0].display());
    }

    #[test]
    fn cleanup_workdir_keep_db_renames_beside_archive() {
        let world = archive_world(true, false, true);
        let db_before = world.config.paths.db_path();
        let parent = crate::config::path_parent(&world.config.paths.archive_path);
        cleanup_workdir(&world.config, CleanupMode::Archive).expect("cleanup");
        assert_kept_only(Some(db_before), &parent, "test");
        assert!(!world.config.paths.work_dir.exists(), "work dir removed");
    }

    #[test]
    fn cleanup_workdir_keep_db_renames_under_extraction_root() {
        // Extract mode: kept_db_parent is the extraction root, not the archive parent.
        let world = ExtractWorld::with_config(|dp| {
            let mut c = crate::config::ExtractConfig::for_scan_test(
                dp.join("archive.tar"),
                dp.join("estage"),
                dp.join("out"),
            );
            c.process.cleanup.db_retention = DbRetention::Always;
            c
        });
        seed_work_dir_files(&world.config);
        let db_before = world.config.paths.db_path();
        let root = world.config.paths.extraction_root().to_path_buf();
        cleanup_workdir(&world.config, CleanupMode::Extract).expect("cleanup");
        assert_kept_only(Some(db_before), &root, "archive");
        assert!(!world.config.paths.work_dir.exists(), "work dir removed");
    }

    #[test]
    fn cleanup_workdir_without_keep_db_removes_database_and_work_dir() {
        let world = archive_world(false, false, true);
        let db_before = world.config.paths.db_path();
        cleanup_workdir(&world.config, CleanupMode::Archive).expect("cleanup");
        assert!(!db_before.is_file(), "db removed");
        assert!(!world.config.paths.work_dir.exists(), "work dir removed");
    }

    #[test]
    fn cleanup_workdir_keep_stage_keeps_work_dir_and_temp_files() {
        let world = archive_world(false, true, true);
        let db_before = world.config.paths.db_path();
        cleanup_workdir(&world.config, CleanupMode::Archive).expect("cleanup");
        assert!(!db_before.is_file(), "db removed");
        assert!(world.config.paths.work_dir.is_dir(), "work dir kept");
        for name in TEMP_NAMES {
            assert!(
                !world.config.paths.work_dir.join(name).exists(),
                "temp file {name} removed"
            );
        }
    }

    #[test]
    fn cleanup_workdir_no_db_materialized_skips_rename_and_removes_work_dir() {
        // No db file -> the keep_db rename arm is skipped, work dir still removed.
        let world = archive_world(true, false, false);
        cleanup_workdir(&world.config, CleanupMode::Archive).expect("cleanup");
        assert!(!world.config.paths.work_dir.exists(), "work dir removed");
    }

    #[test]
    fn cleanup_workdir_absent_work_dir_is_ok() {
        let world = archive_world(false, false, false);
        std::fs::remove_dir_all(&world.config.paths.work_dir).expect("remove work dir");
        cleanup_workdir(&world.config, CleanupMode::Archive).expect("cleanup absent work");
    }

    #[test]
    fn cleanup_workdir_remove_failure_warns_and_keeps_work_dir() {
        if running_as_root() {
            return;
        }
        let world = archive_world(false, false, true);
        // Read-only subtree with a member file makes remove_dir_all fail with
        // EACCES (unlink needs write on the dir holding the file); the helper
        // must warn and keep the work dir in place.
        let readonly = world.config.paths.work_dir.join("sub").join("ro");
        std::fs::create_dir_all(&readonly).expect("create readonly subtree");
        std::fs::write(readonly.join("member.txt"), b"x").expect("write member");
        std::fs::set_permissions(&readonly, std::os::unix::fs::PermissionsExt::from_mode(0o555))
            .expect("chmod ro");
        cleanup_workdir(&world.config, CleanupMode::Archive).expect("cleanup warns");
        assert!(world.config.paths.work_dir.is_dir(), "work dir left behind");
        // Restore permissions so the tempdir drop can clean up.
        std::fs::set_permissions(&readonly, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod back");
    }

    #[test]
    fn reset_workdir_removes_everything_ignoring_keep_flags() {
        let world = archive_world(true, true, true);
        let db_before = world.config.paths.db_path();
        reset_workdir(&world.config).expect("reset");
        assert!(!db_before.is_file(), "db removed");
        assert!(!world.config.paths.work_dir.exists(), "work dir removed despite keep_stage");
    }

    #[test]
    fn reset_workdir_absent_work_dir_is_ok() {
        let world = archive_world(false, false, false);
        std::fs::remove_dir_all(&world.config.paths.work_dir).expect("remove work dir");
        reset_workdir(&world.config).expect("reset absent work");
    }

    #[test]
    fn reset_workdir_remove_failure_propagates() {
        if running_as_root() {
            return;
        }
        let world = archive_world(false, false, true);
        let readonly = world.config.paths.work_dir.join("sub").join("ro");
        std::fs::create_dir_all(&readonly).expect("create");
        std::fs::write(readonly.join("member.txt"), b"x").expect("write member");
        std::fs::set_permissions(&readonly, std::os::unix::fs::PermissionsExt::from_mode(0o555))
            .expect("chmod ro");
        // reset is strict: the failing remove surfaces as an Err (not a warn).
        assert!(reset_workdir(&world.config).is_err(), "reset must fail");
        std::fs::set_permissions(&readonly, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .expect("chmod back");
    }

    #[test]
    fn reset_workdir_removes_temp_files() {
        let world = archive_world(false, true, true); // keep_stage stops the work-dir wipe
        for name in TEMP_NAMES {
            assert!(world.config.paths.work_dir.join(name).is_file(), "{name} seeded");
        }
        reset_workdir(&world.config).expect("reset");
        assert!(
            !world.config.paths.work_dir.exists(),
            "reset wipes work dir regardless of keep flags"
        );
    }

    /// `on-error` retention: a DB with recorded errors is renamed to the kept
    /// location; a clean DB is dropped.
    #[test]
    fn cleanup_on_error_keeps_db_with_recorded_errors() {
        let world = TestWorld::tuned(|c| c.process.cleanup.db_retention = DbRetention::OnError);
        // Materialize the DB and record one error (the on-error count trigger).
        {
            let db = crate::db::Database::open(&world.config.paths.db_path()).expect("open db");
            db.insert_errors(&[crate::db::RecordDraft {
                file_id: None,
                out_tree_id: None,
                phase: crate::db::ErrorPhase::Pipeline(crate::config::PipelinePhase::Hash),
                error: crate::error::FileStatError::general(None, "boom".into()),
                flags: crate::db::flags::ErrorFlags::default(),
            }]).expect("insert error");
        }
        let db_before = world.config.paths.db_path();
        let parent = crate::config::path_parent(&world.config.paths.archive_path);
        cleanup_workdir(&world.config, CleanupMode::Archive).expect("cleanup");
        assert_kept_only(Some(db_before), &parent, "test");
        assert!(!world.config.paths.work_dir.exists(), "work dir removed");
    }

    #[test]
    fn cleanup_on_error_drops_db_without_errors() {
        let world = TestWorld::tuned(|c| c.process.cleanup.db_retention = DbRetention::OnError);
        {
            crate::db::Database::open(&world.config.paths.db_path()).expect("open db");
        }
        let db_before = world.config.paths.db_path();
        cleanup_workdir(&world.config, CleanupMode::Archive).expect("cleanup");
        assert!(!db_before.is_file(), "on-error with zero recorded errors drops the db");
        assert!(!world.config.paths.work_dir.exists(), "work dir removed");
    }
}
