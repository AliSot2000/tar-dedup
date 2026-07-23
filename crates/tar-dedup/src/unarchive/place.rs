//! Place: copy/link cached payloads to final output paths.

use std::fs;
use std::path::{Component, Path, PathBuf};

use filetime::{set_file_mtime, FileTime};

use crate::config::Config;
use crate::db::types::{FilePhase, FileRecord};
use crate::db::Database;
use crate::error::{Error, Result};
use crate::progress::ByteProgress;
use crate::shutdown::Shutdown;

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    let files: Vec<FileRecord> = db.list_files_to_restore()?;
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let progress = ByteProgress::new("extract", total_bytes);

    eprintln!(
        "extract: materializing {} file(s) under {}",
        files.len(),
        config.output_dir.display()
    );

    for record in files {
        shutdown.check_between_files()?;

        let tar_name = record
            .tar_member_name()
            .expect("Invariant Error: FileRecord without sha1 found!");
        let cache_path = config.extract_cache_dir().join(&tar_name);
        if !cache_path.is_file() {
            return Err(Error::Config(format!(
                "missing cached tar member `{tar_name}` for {}",
                record.rel_path.display()
            )));
        }

        let dest = safe_output_path(&config.output_dir, &record.rel_path)?;
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        progress.set_file("extract", &record.rel_path);
        fs::copy(&cache_path, &dest).map_err(|e| Error::io(&dest, e))?;
        // Lightweight mtime/owner until the permissions stage owns full metadata restore.
        apply_basic_metadata(config, &record, &dest)?;
        db.mark_file_phase(record.id, FilePhase::AtDestination)?;
        progress.inc(record.size);
    }

    progress.finish("extract place complete");
    Ok(())
}

pub fn warn_catalog_uncertainty(db: &Database) -> Result<()> {
    let unconfirmed = db.count_unconfirmed_restored()?;
    if unconfirmed > 0 {
        eprintln!(
            "warning: {unconfirmed} restored file(s) were never listed as `archived` in an \
             ingested snapshot (archive may be incomplete or interrupted)"
        );
    }
    Ok(())
}

fn apply_basic_metadata(
    config: &Config,
    record: &FileRecord,
    dest: &Path,
) -> Result<()> {
    if let Some(mtime) = record.mtime {
        let ft = FileTime::from_unix_time(mtime.timestamp(), mtime.timestamp_subsec_nanos());
        let _ = set_file_mtime(dest, ft);
    }

    #[cfg(unix)]
    if config.restore_owner {
        if let (Some(uid), Some(gid)) = (record.uid, record.gid) {
            use std::os::unix::fs::chown;
            if chown(dest, Some(uid), Some(gid)).is_err() {
                tracing::warn!(path = %dest.display(), "chown failed (need root?)");
            }
        }
    }

    Ok(())
}

fn safe_output_path(output_dir: &Path, rel_path: &Path) -> Result<PathBuf> {
    if rel_path.is_absolute() {
        return Err(Error::Config(format!(
            "absolute path in archive catalog: {}",
            rel_path.display()
        )));
    }
    for component in rel_path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(Error::Config(format!(
                "path escapes output directory: {}",
                rel_path.display()
            )));
        }
    }
    Ok(output_dir.join(rel_path))
}
