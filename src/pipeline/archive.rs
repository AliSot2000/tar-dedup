use std::path::Path;

use crate::config::Config;
use crate::db::types::{FileId, FilePhase, FileRecord};
use crate::db::Database;
use crate::error::{Error, Result};
use crate::progress::ByteProgress;
use crate::shutdown::Shutdown;
use crate::tar_writer::TarWriter;

const SNAPSHOT_TAR_NAME: &str = "snapshot.sqlite";

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    let archive_offset = archive_file_len(&config.archive_path);
    let (session_id, _session_start_offset) = match db.open_archive_session()? {
        Some(open) => (open.id, open.archive_offset),
        None => (db.begin_archive_session(archive_offset)?, archive_offset),
    };
    let total_bytes = db.sum_canonical_bytes_to_archive()?;
    let already_archived = db.sum_archived_canonical_bytes()?;

    let progress = ByteProgress::new("archive", total_bytes);
    progress.set_position(already_archived);

    let mut writer = TarWriter::open(
        config.archive_path.clone(),
        config.compression,
        config.jobs,
        config.memlimit_compress,
        shutdown.clone(),
    )?;

    if already_archived == 0 {
        progress.set_message("archive writing snapshot.sqlite (baseline)");
        append_snapshot(&mut writer, config, db, shutdown)?;
    }

    let to_archive = staged_canonical_sorted(db)?;
    if to_archive.is_empty() && already_archived == 0 {
        tracing::warn!("no staged files to archive");
    }

    let mut stopped = false;

    for file_id in to_archive {
        if shutdown.check_between_files().is_err() {
            stopped = true;
            break;
        }

        let Some(record) = db.get_file(file_id)? else {
            continue;
        };
        if record.sha1.is_none() {
            continue;
        }
        let digest = record.sha1.unwrap();
        let tar_name = crate::content_id::content_id_from_digest(
            &digest,
            record.size,
            file_id,
            &record.rel_path,
        )
        .0;
        let source = config.stage_dir().join(&tar_name);

        progress.set_file("archive", &record.rel_path);

        match writer.append_path(&source, &tar_name, shutdown, |n| progress.inc(n)) {
            Ok(()) => {}
            Err(e) if e.is_interrupted() => {
                stopped = true;
                break;
            }
            Err(e) => return Err(e),
        }

        db.mark_file_phase(file_id, FilePhase::Archived)?;
    }

    if stopped {
        if shutdown.is_force() {
            return force_abort_archive(writer, config, db, &progress);
        }

        end_session(
            writer,
            config,
            db,
            shutdown,
            &progress,
            session_id,
        )?;
        progress.abandon();
        return Err(Error::Interrupted);
    }

    end_session(writer, config, db, shutdown, &progress, session_id)?;
    progress.finish("archive complete");
    Ok(())
}

fn force_abort_archive(
    writer: TarWriter,
    config: &Config,
    db: &Database,
    progress: &ByteProgress,
) -> Result<()> {
    writer.abandon();
    remove_archive_file(config)?;
    db.reset_archive_state()?;
    eprintln!(
        "removed archive {}; archive progress reset in work directory",
        config.archive_path.display()
    );
    progress.abandon();
    Err(Error::Interrupted)
}

fn force_abort_without_writer(config: &Config, db: &Database) -> Result<()> {
    remove_archive_file(config)?;
    db.reset_archive_state()?;
    eprintln!(
        "removed archive {}; archive progress reset in work directory",
        config.archive_path.display()
    );
    Err(Error::Interrupted)
}

fn remove_archive_file(config: &Config) -> Result<()> {
    if config.archive_path.is_file() {
        std::fs::remove_file(&config.archive_path)
            .map_err(|e| crate::error::Error::io(&config.archive_path, e))?;
    }
    Ok(())
}

/// Canonical staged files: extension (asc), size (asc), id (asc).
fn staged_canonical_sorted(db: &Database) -> Result<Vec<FileId>> {
    let ids = db.list_canonical_files(FilePhase::Staged)?;
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(record) = db.get_file(id)? else {
            continue;
        };
        items.push((archive_sort_key(&record), id));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(items.into_iter().map(|(_, id)| id).collect())
}

fn archive_sort_key(record: &FileRecord) -> (String, u64, i64) {
    let ext = record
        .rel_path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    (ext, record.size, record.id.0)
}

fn append_snapshot(
    writer: &mut TarWriter,
    config: &Config,
    db: &Database,
    shutdown: &Shutdown,
) -> Result<()> {
    db.checkpoint()?;
    let src = config.db_path();
    let staging = config.work_dir.join(".snapshot-for-tar.sqlite");
    std::fs::copy(&src, &staging).map_err(|e| crate::error::Error::io(&staging, e))?;
    let result = writer.append_path(&staging, SNAPSHOT_TAR_NAME, shutdown, |_| ());
    let _ = std::fs::remove_file(&staging);
    result
}

fn end_session(
    mut writer: TarWriter,
    config: &Config,
    db: &Database,
    shutdown: &Shutdown,
    progress: &ByteProgress,
    session_id: i64,
) -> Result<()> {
    progress.set_message("archive writing snapshot.sqlite (progress)");
    if let Err(e) = append_snapshot(&mut writer, config, db, shutdown) {
        if e.is_interrupted() && shutdown.is_force() {
            writer.abandon();
            return force_abort_without_writer(config, db);
        }
        return Err(e);
    }

    progress.set_message("archive finalizing compression stream");
    match writer.finalize_session(shutdown) {
        Ok((bytes_in, bytes_out)) => {
            db.finalize_archive_session(session_id, bytes_in, bytes_out)?;
            Ok(())
        }
        Err(e) if e.is_interrupted() && shutdown.is_force() => force_abort_without_writer(config, db),
        Err(e) => Err(e),
    }
}

fn archive_file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}
