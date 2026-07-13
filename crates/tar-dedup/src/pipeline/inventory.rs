use std::path::Path;

use chrono::{DateTime, Utc};
use walkdir::WalkDir;

use crate::config::Config;
use crate::db::types::NewFileRecord;
use crate::db::Database;
use crate::error::Result;
use crate::progress::CountProgress;
use crate::shutdown::Shutdown;

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    tracing::info!(root = %config.input_dir.display(), "inventory pass");
    let progress = CountProgress::new("inventory");
    let mut inserted = 0u64;

    for entry in WalkDir::new(&config.input_dir)
        .follow_links(false) // Feature follow symlinks
        .into_iter()
        .filter_map(|e| e.ok())
    {
        shutdown.check_between_files()?;
        // TODO deal with special files.
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let rel = path
            .strip_prefix(&config.input_dir)
            .unwrap_or(path)
            .to_path_buf();
        let meta = std::fs::metadata(path).map_err(|e| crate::error::Error::io(path, e))?;

        if db.insert_file(&NewFileRecord {
            rel_path: rel,
            size: meta.len(),
            mtime: file_mtime(&meta),
            atime: file_atime(&meta),
            uid: file_uid(path),
            gid: file_gid(path),
            mode: Some(file_mode(&meta)),
        })? {
            inserted += 1;
            progress.inc(1);
        }
    }

    progress.finish("inventory complete");
    tracing::info!(inserted, total = db.count_files()?, "inventory indexed");
    Ok(())
}

// TODO: Propagate errors.
fn file_mtime(meta: &std::fs::Metadata) -> Option<DateTime<Utc>> {
    meta.modified().ok().map(DateTime::<Utc>::from)
}

fn file_atime(meta: &std::fs::Metadata) -> Option<DateTime<Utc>> {
    meta.accessed().ok().map(DateTime::<Utc>::from)
}

// Just testing if I can write proper rust code myself
fn file_ctime(meta: &std::fs::Metadata) -> Option<DateTime<Utc>> {
    meta.created().ok().map(DateTime::<Utc>::from)
}

#[cfg(unix)]
fn file_uid(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.uid())
}

#[cfg(not(unix))]
fn file_uid(_path: &Path) -> Option<u32> {
    None
}

#[cfg(unix)]
fn file_gid(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.gid())
}

#[cfg(not(unix))]
fn file_gid(_path: &Path) -> Option<u32> {
    None
}

#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode()
}

#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> u32 {
    0o644
}
