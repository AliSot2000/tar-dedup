use std::fs::OpenOptions;
use std::path::Path;

use crate::archive_footer;
use crate::common::files::warn_if_times_changed;
use crate::config::Config;
use crate::db::flags::FileFlag;
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

    let archive_offset = archive_file_len(&config.archive_path);
    check_archive_bytes_out(db, archive_offset)?;

    debug_assert!(db.open_archive_session()?.is_none());
    let session_id = db.begin_archive_session(archive_offset)?;

    // Require sha1 unless retry_missing_sha asks to include unhashed files.
    let filter_sha = !config.retry_missing_sha;
    db.promote_ineligible_to_archived(filter_sha)?;

    let bytes_in_base = db.get_archive_bytes_in()?;
    let total_bytes = db.sum_canonical_bytes_to_archive(filter_sha)?;
    let already_archived = db.sum_archived_canonical_bytes(filter_sha)?;

    // TODO update eta only when write to buff occurs.
    let progress = ByteProgress::new("archive", total_bytes);
    progress.set_position(already_archived);

    let mut writer = TarWriter::open(
        config.archive_path.clone(),
        &config.compression,
        config.jobs,
        shutdown.clone(),
    )?;

    // Fresh start into archiving.
    if already_archived == 0 {
        progress.set_message(
            &format!("archive writing {SNAPSHOT_INIT_TAR_NAME} (initial manifest)")
        );
        append_snapshot(&mut writer, config, db, shutdown, true)?;
    }

    let to_archive = db.list_staged_canonical_ordered(filter_sha)?;
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

        let record = db.get_file::<StrippedRecord>(file_id)?.expect(
            "File was present in db for listing; missing row means SQL/list bug or DB corruption",
        );
        let tar_name = record.tar_member_name().expect(
            "INVARIANT ERROR: Members to be encoded must have a symlink in the \
            staging directory.",
        );
        let source = config.stage_dir().join(&tar_name);

        // Stage path is a symlink; compare inventory times against the real target.
        let target = std::fs::canonicalize(&source).map_err(|e| Error::io(&source, e))?;
        warn_if_times_changed(&target, record.mtime, record.atime, record.ctime);

        progress.set_file("archive", &record.rel_path);

        match writer.append_path(&source, &tar_name, shutdown, |n| progress.inc(n)) {
            Ok(()) => db.set_flag(record.id, FileFlag::AppendedPath, true)?,
            Err(e) if e.is_interrupted() => {
                stopped = true;
                final_archive = false;
                break;
            }
            Err(e) => {
                tracing::error!(
                    path = &record.rel_path.to_string_lossy(),
                    error = e,
                    "archive append_path failed; marking ErrorWhileArchive and continuing"
                );
                db.set_flag(record.id, FileFlag::ErrorWhileArchive, true)?;
                // Do not set AppendedPath — member was not successfully written.
            }
        }
    }

    // Fast exit on force
    if stopped && shutdown.is_force() {
        return force_abort_session(writer, db, &progress);
    }

    end_session(
        writer,
        config,
        db,
        shutdown,
        &progress,
        session_id,
        bytes_in_base,
        final_archive,
    )?;

    if stopped {
        progress.abandon();
        return Err(Error::Interrupted);
    }

    progress.finish("archive complete");
    Ok(())
}

/// After recovery: if a prior session finalized cleanly, archive length must match meta.
fn check_archive_bytes_out(db: &Database, archive_len: u64) -> Result<()> {
    if !db.has_finalized_archive_session()? {
        return Ok(());
    }
    let Some(expected) = db.get_archive_bytes_out()? else {
        return Ok(());
    };
    if archive_len != expected {
        return Err(Error::Config(format!(
            "archive file length {archive_len} does not match recorded archive_bytes_out {expected} \
             (file truncated or modified externally)"
        )));
    }
    Ok(())
}

/// Truncate archive to the incomplete session's start offset, mark session aborted,
/// and clear [`FileFlag::AppendedPath`] on non-`archived` rows only.
/// Prior finalized sessions (and their archived files, including sticky `AppendedPath`) stay intact.
fn recover_incomplete_session(config: &Config, db: &Database) -> Result<()> {
    let open_session = match db.open_archive_session()? {
        None => return Ok(()),
        Some(s) => s,
    };

    truncate_archive_at(&config.archive_path, open_session.archive_offset)?;
    db.abort_incomplete_archive_session(&config.archive_path, &open_session)?;

    eprintln!(
        "recovered incomplete archive session at offset {} ({})",
        open_session.archive_offset,
        config.archive_path.display()
    );
    Ok(())
}

/// Truncate archive to `offset` (end of previous finished stream / start of incomplete one).
fn truncate_archive_at(path: &Path, offset: u64) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| Error::io(path, e))?;
    file.set_len(offset).map_err(|e| Error::io(path, e))?;
    file.sync_all().map_err(|e| Error::io(path, e))?;
    if offset == 0 {
        // Empty archive file: remove so next session starts clean.
        drop(file);
        let _ = std::fs::remove_file(path);
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

/// append_snapshot, commits the db, stages it, adds it to the archive and removes the stage again.
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
    std::fs::copy(&src, &staging).map_err(|e| Error::io(&staging, e))?;
    let tar_dst = if is_init { SNAPSHOT_INIT_TAR_NAME } else { SNAPSHOT_TAR_NAME };
    // INFO: append_path might return return interrupted error!
    let result = writer.append_path(&staging, tar_dst, shutdown, |_| ());
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
    bytes_in_base: u64,
    write_tar_eof: bool,
) -> Result<()> {
    db.promote_pending_archived()?;
    // Full archive pass only: every remaining row has been considered (or was ineligible).
    if write_tar_eof {
        db.promote_remainder_to_archived()?;
    }
    db.stamp_archive_session_finished_at(session_id)?;

    progress.set_message(format!("archive writing {SNAPSHOT_TAR_NAME} (progress)").as_str());
    if let Err(e) = append_snapshot(&mut writer, config, db, shutdown, false) {
        if e.is_interrupted() && shutdown.is_force() {
            return force_abort_session(writer, db, progress);
        }
        return Err(e);
    }

    progress.set_message("archive finalizing compression stream");
    let result = if write_tar_eof {
        // ARCHIVE!!!
        writer.finalize_archive(shutdown)
    } else {
        // SESSION!!!
        writer.finalize_session(shutdown)
    };

    match result {
        Ok((session_bytes_in, bytes_out)) => {
            db.finalize_archive_session(session_id)?;
            db.set_archive_bytes_in(bytes_in_base.saturating_add(session_bytes_in))?;
            db.set_archive_bytes_out(bytes_out)?;

            if write_tar_eof && config.write_archive_footer {
                db.checkpoint()?;
                archive_footer::write_footer(&config.archive_path, &config.db_path())?;
            }
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
