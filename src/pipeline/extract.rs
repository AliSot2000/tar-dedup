use std::fs;
use std::io::copy;
use std::path::{Component, Path, PathBuf};

use filetime::{set_file_mtime, FileTime};

use crate::config::{Config, ExtractPipelinePhase, ExtractRuntimeState};
use crate::db::types::FilePhase;
use crate::db::Database;
use crate::error::{Error, Result};
use crate::progress::ByteProgress;
use crate::shutdown::Shutdown;
use crate::tar_reader::open_tar_archive;

const SNAPSHOT_TAR_NAME: &str = "snapshot.sqlite";

pub fn run(config: Config, shutdown: Shutdown) -> Result<()> {
    if config.fresh {
        reset_extract_work(&config)?;
    }

    let db_path = config.db_path();
    let mut state = load_extract_state(&db_path)?;

    if state.phase == ExtractPipelinePhase::Done {
        eprintln!(
            "extract already complete: {}",
            config.output_dir.display()
        );
        return Ok(());
    }

    if state.phase == ExtractPipelinePhase::ScanTar {
        eprintln!("extract: scanning archive");
        let db = scan_tar(&config, &db_path, &shutdown)?;
        state.phase = ExtractPipelinePhase::Place;
        db.save_extract_runtime_state(&state)?;
    }

    let db = Database::open(&db_path)?;

    if state.phase == ExtractPipelinePhase::Place {
        materialize(&config, &db, &shutdown)?;
        warn_catalog_uncertainty(&db)?;
        state.phase = ExtractPipelinePhase::Cleanup;
        db.save_extract_runtime_state(&state)?;
    }

    if state.phase == ExtractPipelinePhase::Cleanup {
        cleanup_extract_cache(&config)?;
        state.phase = ExtractPipelinePhase::Done;
        db.save_extract_runtime_state(&state)?;
    }

    eprintln!("extracted to {}", config.output_dir.display());
    Ok(())
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

fn reset_extract_work(config: &Config) -> Result<()> {
    let cache = config.extract_cache_dir();
    if cache.is_dir() {
        fs::remove_dir_all(&cache).map_err(|e| Error::io(&cache, e))?;
    }
    let db_path = config.db_path();
    if db_path.is_file() {
        fs::remove_file(&db_path).map_err(|e| Error::io(&db_path, e))?;
    }
    let tmp = config.work_dir.join(".snapshot-ingest.tmp");
    if tmp.is_file() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(())
}

/// Walk the tar stream: load initial manifest, cache payloads, ingest snapshot confirmations.
fn scan_tar(config: &Config, db_path: &Path, shutdown: &Shutdown) -> Result<Database> {
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
    eprintln!("extract: manifest loaded, {paths} path(s) cached and unarchived, {snapshots} snapshot(s) seen");

    Ok(db)
}

fn materialize(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    let files = db.list_files_to_restore()?;
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let progress = ByteProgress::new("extract", total_bytes);

    eprintln!(
        "extract: materializing {} file(s) under {}",
        files.len(),
        config.output_dir.display()
    );

    for record in files {
        shutdown.check_between_files()?;

        let tar_name = db.tar_member_path(&record)?;
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
        apply_metadata(config, &record, &dest)?;
        db.mark_file_phase(record.id, FilePhase::AtDestination)?;
        progress.inc(record.size);
    }

    progress.finish("extract complete");
    Ok(())
}

fn warn_catalog_uncertainty(db: &Database) -> Result<()> {
    let unconfirmed = db.count_unconfirmed_restored()?;
    if unconfirmed > 0 {
        eprintln!(
            "warning: {unconfirmed} restored file(s) were never listed as `archived` in an \
             ingested snapshot (archive may be incomplete or interrupted)"
        );
    }
    Ok(())
}

fn apply_metadata(
    config: &Config,
    record: &crate::db::types::FileRecord,
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

fn cleanup_extract_cache(config: &Config) -> Result<()> {
    let cache = config.extract_cache_dir();
    if cache.is_dir() {
        fs::remove_dir_all(&cache).map_err(|e| Error::io(&cache, e))?;
    }
    let tmp = config.work_dir.join(".snapshot-ingest.tmp");
    if tmp.is_file() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(())
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
