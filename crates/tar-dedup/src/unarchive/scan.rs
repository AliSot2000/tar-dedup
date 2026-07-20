//! Scan/untar: walk the archive stream, cache payloads, ingest snapshot.sqlite.

use std::fs;
use std::io::copy;
use std::path::Path;

use crate::config::Config;
use crate::db::Database;
use crate::error::{Error, Result};
use crate::shutdown::Shutdown;
use crate::tar_reader::open_tar_archive;

const SNAPSHOT_TAR_NAME: &str = "snapshot.sqlite";

/// Walk the tar stream: load initial manifest, cache payloads, ingest snapshot confirmations.
pub fn run(config: &Config, db_path: &Path, shutdown: &Shutdown) -> Result<Database> {
    fs::create_dir_all(config.extract_cache_dir())
        .map_err(|e| Error::io(&config.extract_cache_dir(), e))?;

    let mut archive = open_tar_archive(&config.archive_path, config.compression)?;
    let snapshot_tmp = config.work_dir.join(".snapshot-ingest.tmp");
    let mut manifest_loaded = db_path.is_file();
    let mut snapshots = 0u32;
    let mut db = if manifest_loaded {
        Some(Database::open(db_path)?)
    } else {
        None
    };

    for entry in archive.entries().map_err(|e| Error::io(&config.archive_path, e))? {
        shutdown.check_between_files()?;
        let mut entry = entry.map_err(|e| Error::io(&config.archive_path, e))?;
        let path = entry
            .path()
            .map_err(|e| Error::Other(anyhow::anyhow!("tar entry path: {e}")))?;
        let name = entry_name(&path)?;

        if name == SNAPSHOT_TAR_NAME {
            let mut out = fs::File::create(&snapshot_tmp)
                .map_err(|e| Error::io(&snapshot_tmp, e))?;
            copy(&mut entry, &mut out).map_err(|e| Error::io(&snapshot_tmp, e))?;

            if !manifest_loaded {
                Database::install_initial_manifest(&snapshot_tmp, db_path)?;
                let opened = Database::open(db_path)?;
                opened.init_extract_runtime_state()?;
                db = Some(opened);
                manifest_loaded = true;
            } else {
                let d = db.as_ref().expect("manifest loaded");
                d.apply_snapshot_archived_flags(&snapshot_tmp)?;
            }
            let d = db.as_ref().expect("manifest loaded");
            snapshots = d.record_snapshot_ingested()?;
            continue;
        }

        if !manifest_loaded {
            return Err(Error::Config(format!(
                "tar member `{name}` before initial `{SNAPSHOT_TAR_NAME}`; not a tar-dedup archive?"
            )));
        }

        let dest = config.extract_cache_dir().join(&name);
        if let Some(parent) = dest.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
            }
        }
        entry
            .unpack(&dest)
            .map_err(|e| Error::io(&dest, e))?;

        db.as_ref()
            .expect("manifest loaded")
            .promote_cached_tar_member(&name)?;
    }

    if snapshots == 0 {
        return Err(Error::Config(format!(
            "archive missing embedded `{SNAPSHOT_TAR_NAME}`; not a tar-dedup archive?"
        )));
    }

    let db = db.ok_or_else(|| {
        Error::Config(format!(
            "archive missing embedded `{SNAPSHOT_TAR_NAME}`; not a tar-dedup archive?"
        ))
    })?;

    let paths = db.list_files_to_restore()?.len();
    eprintln!(
        "extract: manifest loaded, {paths} path(s) cached and unarchived, {snapshots} snapshot(s) seen"
    );

    Ok(db)
}

fn entry_name(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::Config("invalid tar entry name".into()))?;
    if name.contains('/') || name.contains('\\') || name == ".." || name == "." {
        return Err(Error::Config(format!("unsupported tar entry name: {name}")));
    }
    Ok(name.to_string())
}
