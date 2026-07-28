use std::path::Path;

use crate::common::files::warn_if_times_changed;
use crate::config::Config;
use crate::db::types::{ArchiveSession, StrippedRecord};
use crate::db::Database;
use crate::error::{Error, Result};
use crate::progress::ByteProgress;
use crate::shutdown::Shutdown;
use crate::tar_writer::TarWriter;

const SNAPSHOT_INIT_TAR_NAME: &str = "manifest.sqlite";
const SNAPSHOT_TAR_NAME: &str = "snapshot.sqlite";

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    // Crash / force leftover: truncate incomplete stream C, keep finished A..B.
    recover_incomplete_session(config, db)?;

    // TODO really? Is there a case when there's an offset we want to keep from the physical?
    let archive_offset = archive_file_len(&config.archive_path);
    let (session_id, _session_start_offset) = match db.open_archive_session()? {
        Some(open) => (open.id, open.archive_offset),
        None => (db.begin_archive_session(archive_offset)?, archive_offset),
    };
    let total_bytes = db.sum_canonical_bytes_to_archive()?;
    let already_archived = db.sum_archived_canonical_bytes()?;

    // TODO update eta only when write to buff occurs.
    let progress = ByteProgress::new("archive", total_bytes);
    progress.set_position(already_archived);

    let mut writer = TarWriter::open(
        config.archive_path.clone(),
        config.compression,
        config.jobs,
        config.memlimit_compress,
        shutdown.clone(),
    )?;

    // Fresh start into archiving.
    if already_archived == 0 {
        progress.set_message("archive writing manifest.sqlite (baseline)");
        append_snapshot(&mut writer, config, db, shutdown, true)?;
    }

    let to_archive = staged_canonical_sorted(db)?;
    if to_archive.is_empty() && already_archived == 0 {
        tracing::warn!("no staged files to archive");
    }

    let mut stopped = false;
    let mut final_archive = true;

    for file_id in to_archive {
        if shutdown.check_between_files().is_err() {
            stopped = true;
            final_archive = false;
            break;
        }

        let record = db
            .get_file::<StrippedRecord>(file_id)?
            .expect(
                "File was present in db for listing, error in sql statement \
            or database corrupted.",
            );

        let tar_name = record
            .tar_member_name()
            .expect("Encoding to base64 failed or invalid record provided");
        let source = config.stage_dir().join(&tar_name);
        let target = std::fs::canonicalize(&source).map_err(|e| Error::io(&source, e))?;
        warn_if_times_changed(&target, record.mtime, record.atime, record.ctime);

        progress.set_file("archive", &record.rel_path);

        match writer.append_path(&source, &tar_name, Some(file_id), shutdown, |n| {
            progress.inc(n)
        }) {
            Ok(()) => {
                // Durable until finalize (→ archived) or startup recover (cleared).
                db.mark_archive_session_pending(file_id)?;
            }
            Err(e) if e.is_interrupted() => {
                stopped = true;
                final_archive = false;
                break;
            }
            Err(e) => return Err(e),
        }
    }

    // Fast exit on force
    if stopped && shutdown.is_force() {
        return force_abort_session(writer, db, &progress);
    }

    // Perform Graceful cleanup.
    end_session(
        writer,
        config,
        db,
        shutdown,
        &progress,
        session_id,
        final_archive,
    )?;

    if stopped {
        progress.abandon();
        return Err(Error::Interrupted);
    }

    progress.finish("archive complete");
    Ok(())
}

/// Truncate archive to the incomplete session's start offset, mark session aborted,
/// and clear [`crate::db::flags::FileFlag::ArchiveSessionPending`].
/// Prior finalized sessions (and their archived files) stay intact.
fn abort_session_at(
    config: &Config,
    db: &Database,
    session: &ArchiveSession,
) -> Result<()> {
    let offset = session.archive_offset;
    db.abort_incomplete_archive_session(&config.archive_path, session)?;
    eprintln!(
        "recovered incomplete archive session at offset {offset} ({})",
        config.archive_path.display()
    );
    Ok(())
}

fn recover_incomplete_session(config: &Config, db: &Database) -> Result<()> {
    if let Some(open) = db.open_archive_session()? {
        abort_session_at(config, db, &open)?;
    }
    Ok(())
}

/// Force abort: abandon the writer in place. Leave session `finalized = 0` and
/// pending file flags; next run's startup recovery truncates and marks aborted.
fn force_abort_session(
    writer: TarWriter,
    db: &Database,
    progress: &ByteProgress,
) -> Result<()> {
    writer.abandon();
    // Ensure pending flags + open session are durable before exit.
    db.checkpoint()?;
    progress.abandon();
    Err(Error::Interrupted)
}

fn staged_canonical_sorted(db: &Database) -> Result<Vec<FileId>> {
    let ids = db.list_canonical_files(FilePhase::Staged)?;
    let mut items = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(record) = db.get_file::<StrippedRecord>(id)? else {
            continue;
        };
        items.push((archive_sort_key(&record), id));
    }
    items.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(items.into_iter().map(|(_, id)| id).collect())
}

fn archive_sort_key(record: &StrippedRecord) -> (String, u64, i64) {
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
    is_init: bool,
) -> Result<()> {

    db.checkpoint()?;
    let src = config.db_path();
    let staging = config.work_dir.join(".snapshot-for-tar.sqlite");
    std::fs::copy(&src, &staging).map_err(|e| crate::error::Error::io(&staging, e))?;
    let tar_dst = if is_init { SNAPSHOT_INIT_TAR_NAME } else { SNAPSHOT_TAR_NAME };
    let result = writer.append_path(&staging, tar_dst, None, shutdown, |_| ());
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
    write_tar_eof: bool,
) -> Result<()> {
    progress.set_message("archive writing snapshot.sqlite (progress)");
    if let Err(e) = append_snapshot(&mut writer, config, db, shutdown) {
        if e.is_interrupted() && shutdown.is_force() {
            return force_abort_session(writer, db, progress);
        }
        return Err(e);
    }

    progress.set_message("archive finalizing compression stream");
    // TODO makes no sense.
    let result = if write_tar_eof {
        writer.finalize_archive(shutdown)
    } else {
        writer.finalize_session(shutdown)
    };

    match result {
        Ok((bytes_in, bytes_out, members)) => {
            db.mark_files_archived(&members)?;
            db.finalize_archive_session(session_id, bytes_in, bytes_out)?;
            Ok(())
        }
        Err(e) if e.is_interrupted() && shutdown.is_force() => {
            // Writer already dropped mid-finalize; leave open session + pending for
            // next startup recovery (same as force abort / crash).
            db.checkpoint()?;
            progress.abandon();
            Err(Error::Interrupted)
        }
        Err(e) => Err(e),
    }
}

fn archive_file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}
