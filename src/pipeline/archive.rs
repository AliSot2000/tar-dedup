use std::fs::OpenOptions;
use std::path::Path;

use crate::config::Config;
use crate::db::types::FilePhase;
use crate::db::Database;
use crate::error::{Error, Result};
use crate::progress::ByteProgress;
use crate::shutdown::Shutdown;
use crate::tar_writer::TarWriter;

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    let archive_offset = archive_file_len(&config.archive_path);
    let (session_id, session_start_offset) = match db.open_archive_session()? {
        Some(open) => (open.id, open.archive_offset),
        None => (db.begin_archive_session(archive_offset)?, archive_offset),
    };
    let total_bytes = db.sum_canonical_bytes_to_archive()?;
    let already_archived = db.sum_archived_canonical_bytes()?;

    let progress = ByteProgress::new("archive", total_bytes);
    progress.set_position(already_archived);

    let mut writer =
        TarWriter::open(config.archive_path.clone(), config.compression, session_id)?;

    let to_archive = db.list_canonical_files(FilePhase::Staged)?;
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
            drop(writer);
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

        progress.set_message("archive finalizing compression stream");
        match writer.finalize_session(shutdown) {
            Ok((bytes_in, bytes_out)) => {
                db.finalize_archive_session(session_id, bytes_in, bytes_out)?;
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
                progress.abandon();
                return Err(Error::Interrupted);
            }
            Err(e) => return Err(e),
        }
        progress.abandon();
        return Err(Error::Interrupted);
    }

    progress.set_message("archive finalizing compression stream");
    let (bytes_in, bytes_out) = writer.finalize_session(shutdown)?;
    db.finalize_archive_session(session_id, bytes_in, bytes_out)?;
    progress.finish("archive complete");
    Ok(())
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
