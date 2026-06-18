use std::fs::OpenOptions;
use std::path::Path;

use crate::config::{CompressionFormat, Config};
use crate::db::types::{FileId, FilePhase, FileRecord};
use crate::db::Database;
use crate::error::{Error, Result};
use crate::progress::ByteProgress;
use crate::shutdown::Shutdown;
use crate::tar_writer::TarWriter;

const SNAPSHOT_TAR_NAME: &str = "snapshot.sqlite";

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    if config.compression == CompressionFormat::Xz {
        eprintln!(
            "xz compression: preset -{}, {} threads",
            crate::compression::XZ_PRESET,
            config.jobs
        );
    }

    let archive_offset = archive_file_len(&config.archive_path);
    let (session_id, session_start_offset) = match db.open_archive_session()? {
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
        let tar_name =
            crate::content_id::content_id_from_digest(&digest, record.size, &record.rel_path).0;
        let source = config.stage_dir().join(&tar_name);

        progress.set_file("archive", &record.rel_path.to_string_lossy());

        match writer.append_path(&source, &tar_name, shutdown, |n| progress.inc(n)) {
            Ok(()) => {}
            Err(Error::Interrupted) => {
                stopped = true;
                break;
            }
            Err(e) => return Err(e),
        }

        let entry_id = db.queue_archive_entry(file_id, session_id, &tar_name)?;
        db.mark_entry_done(entry_id)?;
        db.mark_file_phase(file_id, FilePhase::Archived)?;
    }

    if stopped {
        if shutdown.is_force() {
            writer.abandon();
            truncate_archive(&config.archive_path, session_start_offset)?;
            db.abandon_archive_session(session_id)?;
            if session_start_offset == 0 {
                let _ = std::fs::remove_file(&config.archive_path);
            } else {
                eprintln!(
                    "removed incomplete compression stream from {}",
                    config.archive_path.display()
                );
            }
            progress.abandon();
            return Err(Error::Interrupted);
        }

        end_session(
            writer,
            config,
            db,
            shutdown,
            &progress,
            session_id,
            session_start_offset,
        )?;
        progress.abandon();
        return Err(Error::Interrupted);
    }

    end_session(
        writer,
        config,
        db,
        shutdown,
        &progress,
        session_id,
        session_start_offset,
    )?;
    progress.finish("archive complete");
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
    session_start_offset: u64,
) -> Result<()> {
    progress.set_message("archive writing snapshot.sqlite (progress)");
    append_snapshot(&mut writer, config, db, shutdown)?;

    progress.set_message("archive finalizing compression stream");
    match writer.finalize_session(shutdown) {
        Ok((bytes_in, bytes_out)) => {
            db.finalize_archive_session(session_id, bytes_in, bytes_out)?;
            Ok(())
        }
        Err(Error::Interrupted) if shutdown.is_force() => {
            truncate_archive(&config.archive_path, session_start_offset)?;
            db.abandon_archive_session(session_id)?;
            if session_start_offset == 0 {
                let _ = std::fs::remove_file(&config.archive_path);
            } else {
                eprintln!(
                    "removed incomplete compression stream from {}",
                    config.archive_path.display()
                );
            }
            Err(Error::Interrupted)
        }
        Err(e) => Err(e),
    }
}

fn archive_file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

fn truncate_archive(path: &Path, len: u64) -> Result<()> {
    if !path.is_file() {
        return Ok(());
    }
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| crate::error::Error::io(path, e))?;
    file.set_len(len)
        .map_err(|e| crate::error::Error::io(path, e))?;
    Ok(())
}
