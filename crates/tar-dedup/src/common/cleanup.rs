//! Shared work-directory cleanup for archive (`.astage`) and extract (`.estage`).

use std::fs;
use std::path::PathBuf;

use chrono::Utc;

use crate::config::{archive_stem, WorkLayout};
use crate::error::{Error, Result};

pub use crate::config::CleanupMode;

/// Never touches archive or extraction trees (except relocating a kept DB).
pub fn cleanup_workdir(config: &impl WorkLayout, mode: CleanupMode) -> Result<()> {
    let db_path = config.paths().db_path();
    let keep = config.cleanup();

    if keep.keep_db && db_path.is_file() {
        let dest = retained_db_path(config, mode);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        fs::rename(&db_path, &dest).map_err(|e| Error::io(&dest, e))?;
        eprintln!("kept database: {}", dest.display());
    } else if db_path.is_file() {
        fs::remove_file(&db_path).map_err(|e| Error::io(&db_path, e))?;
    }

    remove_temp_files(config);

    if !keep.keep_stage {
        let work = &config.paths().work_dir;
        if work.is_dir() {
            fs::remove_dir_all(work).map_err(|e| Error::io(work, e))?;
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

fn retained_db_path(config: &impl WorkLayout, mode: CleanupMode) -> PathBuf {
    let stem = archive_stem(&config.paths().archive_path);
    let ts = Utc::now().to_rfc3339().replace(':', "-");
    let name = format!("{ts}_{stem}.sqlite");
    config.kept_db_parent(mode).join(name)
}
