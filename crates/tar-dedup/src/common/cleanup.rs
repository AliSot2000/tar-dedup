//! Shared work-directory cleanup for archive (`.astage`) and extract (`.estage`).

use std::fs;
use std::path::PathBuf;

use chrono::Utc;

use crate::config::{archive_stem, path_parent, Config};
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupMode {
    Archive,
    Extract,
}

/// Never touches `archive_path` or `output_dir` trees (except relocating a kept DB into `output_dir`).
///
/// Keep flags come from `config.cleanup` (same nesting idea as `config.compression`).
pub fn cleanup_workdir(config: &Config, mode: CleanupMode) -> Result<()> {
    let db_path = config.db_path();
    let keep = &config.cleanup;

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
        let work = &config.work_dir;
        if work.is_dir() {
            fs::remove_dir_all(work).map_err(|e| Error::io(work, e))?;
        }
    }

    Ok(())
}

/// Wipe work dir for `--fresh` (ignores keep flags).
pub fn reset_workdir(config: &Config) -> Result<()> {
    let db_path = config.db_path();
    if db_path.is_file() {
        fs::remove_file(&db_path).map_err(|e| Error::io(&db_path, e))?;
    }
    remove_temp_files(config);
    let work = &config.work_dir;
    if work.is_dir() {
        fs::remove_dir_all(work).map_err(|e| Error::io(work, e))?;
    }
    Ok(())
}

/// Remove the common temp files: .snapshot-ingest.tmp, .snapshot-for-tar.sqlite, .lock
fn remove_temp_files(config: &Config) -> () {
    for name in [".snapshot-ingest.tmp", ".snapshot-for-tar.sqlite", ".lock"] {
        let p = config.work_dir.join(name);
        if p.is_file() {
            let _ = fs::remove_file(&p);
        }
    }
}

fn retained_db_path(config: &Config, mode: CleanupMode) -> PathBuf {
    let stem = archive_stem(&config.archive_path);
    let ts = Utc::now().to_rfc3339().replace(':', "-");
    let name = format!("{ts}_{stem}.sqlite");
    let parent = match mode {
        CleanupMode::Archive => path_parent(&config.archive_path),
        CleanupMode::Extract => config.output_dir.as_path(),
    };
    parent.join(name)
}
