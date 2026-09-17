use std::fs::OpenOptions;
use std::io;
use std::path::Path;

use crate::archive::ArchiveRTArgs;
use crate::archive_footer;
use crate::common::files::warn_if_times_changed;
use crate::common::{SNAPSHOT_INIT_TAR_NAME, SNAPSHOT_TAR_NAME};
use crate::db::ErrorPhase;
use crate::db::flags::{ErrorFlags, FileFlag};
use crate::db::types::StrippedRecord;
use crate::db::{Database, Recorder};
use crate::error::{Error, FileStatError, Result};
use crate::progress::ProgressBarSet;
use crate::tar_writer::TarWriter;
const ERROR_PHASE: ErrorPhase = ErrorPhase::Pipeline(crate::config::PipelinePhase::Archive);

// TODO Consider the transition state of the files that are ingested.

pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    let config = rt.config;
    let db = rt.db;
    let shutdown = rt.shutdown;
    let mut recorder = Recorder::new(db, !config.process.no_errors);
    // Crash / force leftover: truncate incomplete stream C, keep finished A..B.
    recover_incomplete_session(rt, &mut recorder)?;

    let archive_offset = archive_file_len(&config.paths.archive_path);
    check_archive_bytes_out(db, archive_offset)?;

    debug_assert!(db.open_archive_session()?.is_none());
    let session_id = db.begin_archive_session(archive_offset)?;

    // Require sha1 unless retry_missing_sha asks to include unhashed files.
    let filter_sha = !config.pipeline.retry_missing_sha;
    let promoted = db.promote_ineligible_to_archived(filter_sha)?;
    rt.progress.inc_global(promoted);

    let bytes_in_base = db.get_archive_bytes_in()?;
    let total_bytes = db.sum_canonical_bytes_to_archive(filter_sha)?;
    let already_archived = db.sum_archived_canonical_bytes(filter_sha)?;

    // TODO update eta only when write to buff occurs.
    let progress = rt.progress;
    progress.set_phase_total(total_bytes);
    progress.set_phase_position(already_archived);

    let mut writer = TarWriter::open(
        config.paths.archive_path.clone(),
        &config.compression,
        config.process.jobs,
        shutdown.clone(),
        true, // sparse
    )?;

    // Fresh start into archiving.
    if already_archived == 0 {
        progress.set_phase_msg(&format!(
            "archive writing {SNAPSHOT_INIT_TAR_NAME} (initial manifest)"
        ));
        append_snapshot(&mut writer, rt, true, &mut recorder)?;
    }

    // TODO add batching
    let to_archive = db.list_staged_canonical_ordered(filter_sha)?;
    if to_archive.is_empty() && already_archived == 0 {
        tracing::warn!("no staged files to archive");
    }

    let capture_error = |err: Error, rec: &mut Recorder, fid| {
        match err {
            Error::FileStat(e) => rec.record_file(
                fid, ERROR_PHASE, e, ErrorFlags::default(),
            ),
            _ => panic!("PRECONDITION FAILED: Function may only process variant FileStatError")
        }

    };

    let mut stopped = false;
    let mut final_archive = true;

    for file_id in to_archive {
        if shutdown.check_between_files().is_err() {
            stopped = true;
            final_archive = false;
            break;
        }

        let record = db.get_file_by_id::<StrippedRecord>(file_id)?.expect(
            "File was present in db for listing; missing row means SQL/list bug or DB corruption",
        );
        let tar_name = record.tar_member_name().expect(
            "INVARIANT ERROR: Members to be encoded must have a symlink in the \
            staging directory.",
        );
        let source = config.paths.stage_dir().join(&tar_name);

        // Stage path is a symlink; compare inventory times against the real target.
        let target = match std::fs::canonicalize(&source) {
            Ok(t) => t,
            Err(e) => {
                let err = Error::io(&source, e);
                capture_error(err, &mut recorder, file_id);
                db.set_file_flag(record.id, FileFlag::ErrorWhileArchive, true)?;
                // return Err(err); // TODO why was here error?
                continue;
            }
        };
        warn_if_times_changed(&target, record.mtime, record.atime, record.ctime);

        progress.set_phase_file("archive", &record.abs_path);

        match writer.append_path(&source, &tar_name, shutdown, |n| progress.inc_phase(n)) {
            Ok(()) => {
                db.set_file_flag(record.id, FileFlag::AppendedPath, true)?;
                progress.inc_both(1);
            }
            Err(e) if e.is_interrupted() => {
                stopped = true;
                final_archive = false;
                break;
            }
            Err(e) => {
                assert!(matches!(e, Error::FileStat(_)),
                        "Unexpected return type, only FileStatError Expected");
                tracing::error!(
                    path = %record.abs_path.to_string_lossy(),
                    error = %e,
                    "archive append_path failed; marking ErrorWhileArchive and continuing"
                );
                capture_error(e, &mut recorder, file_id);
                db.set_file_flag(record.id, FileFlag::ErrorWhileArchive, true)?;
                // Do not set AppendedPath — member was not successfully written.
            }
        }
    }

    // Fast exit on force
    if stopped && shutdown.is_force() {
        return force_abort_session(writer, db, progress);
    }

    end_session(
        writer,
        rt,
        progress,
        session_id,
        bytes_in_base,
        final_archive,
        &mut recorder,
    )?;

    if stopped {
        return Err(Error::Interrupted);
    }

    recorder.flush()?;
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
/// Recover from an incomplete session; the whole archive file is session-scoped,
/// so failures are recorded against the session (not any single file).
fn recover_incomplete_session(
    rt: &ArchiveRTArgs,
    recorder: &mut Recorder)
    -> Result<()> {
    let config = rt.config;
    let db = rt.db;
    let open_session = match db.open_archive_session()? {
        None => return Ok(()),
        Some(s) => s,
    };

    let res = truncate_archive_at(
        &config.paths.archive_path,
        open_session.archive_offset
    ).map_err(|e| FileStatError::io(&config.paths.archive_path, e));

    match res {
        Ok(_) => (),
        Err(e) => {
            recorder.record_session(ERROR_PHASE, e.recreate(), ErrorFlags::default());
            return Err(Error::FileStat(e));
        }
    }

    db.abort_incomplete_archive_session(&open_session)?;

    tracing::info!(
        "recovered incomplete archive session at offset {} ({})",
        open_session.archive_offset,
        config.paths.archive_path.display()
    );
    Ok(())
}

/// Truncate archive to `offset` (end of previous finished stream / start of incomplete one).
fn truncate_archive_at(path: &Path, offset: u64) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let file = OpenOptions::new()
        .write(true)
        .open(path)?;
    file.set_len(offset)?;
    file.sync_all()? ;
    if offset == 0 {
        // Empty archive file: remove so next session starts clean.
        drop(file);
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Force abort: abandon the writer in place. Leave session `finalized = 0` and
/// pending file flags; next run's startup recovery truncates and marks aborted.
fn force_abort_session(writer: TarWriter, db: &Database, progress: &ProgressBarSet) -> Result<()> {
    writer.abandon();
    // Ensure pending flags + open session are durable before exit.
    db.checkpoint()?;
    progress.abandon();
    Err(Error::Interrupted)
}

/// append_snapshot, commits the db, stages it, adds it to the archive and removes the stage again.
fn append_snapshot(
    writer: &mut TarWriter,
    rt: &ArchiveRTArgs,
    is_init: bool,
    recorder: &mut Recorder)
    -> Result<()> {

    let config = rt.config;
    let db = rt.db;
    let shutdown = rt.shutdown;
    db.checkpoint()?;
    let src = config.paths.db_path();
    let staging = config.paths.stage_archive_snapshot();
    let mut capture_error = |err: &io::Error, path: &Path| {
        recorder.record_session(
            ERROR_PHASE,
            FileStatError::Io {
                path: path.to_path_buf(),
                source: io::Error::new(err.kind(), err.to_string())
            },
            ErrorFlags::default(),
        )
    };
    match std::fs::copy(&src, &staging) {
        Ok(_) => (),
        Err(e) => {
            capture_error(&e, &staging);
            return Err(Error::io(&staging, e));
        }
    }
    let tar_dst = if is_init { SNAPSHOT_INIT_TAR_NAME } else { SNAPSHOT_TAR_NAME };
    // INFO: append_path might return return interrupted error!
    let result = writer
        .append_path(&staging, tar_dst, shutdown, |_| ());
    match std::fs::remove_file(&staging){
        Ok(_) => (),
        Err(e) => capture_error(&e, &staging),
    };
    result
}

fn end_session(
    mut writer: TarWriter,
    rt: &ArchiveRTArgs,
    progress: &ProgressBarSet,
    session_id: i64,
    bytes_in_base: u64,
    write_tar_eof: bool,
    recorder: &mut Recorder,
) -> Result<()> {
    let config = rt.config;
    let db = rt.db;
    let shutdown = rt.shutdown;
    let n_pending = db.promote_pending_archived()?;
    progress.inc_global(n_pending);
    // Full archive pass only: every remaining row has been considered (or was ineligible).
    if write_tar_eof {
        let n_rem = db.promote_remainder_to_archived()?;
        progress.inc_global(n_rem);
    }
    db.stamp_archive_session_finished_at(session_id)?;

    progress.set_phase_msg(&format!("archive writing {SNAPSHOT_TAR_NAME} (progress)"));
    if let Err(e) = append_snapshot(&mut writer, rt, false, recorder) {
        if e.is_interrupted() && shutdown.is_force() {
            return force_abort_session(writer, db, progress);
        }
        return Err(e);
    }

    progress.set_phase_msg("archive finalizing compression stream");
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

            if write_tar_eof && config.pipeline.write_archive_footer {
                // TODO ensure every entry has phase='archived' the database is in a consistent
                //   state.
                if config.pipeline.clear_archive_meta {
                    db.clear_archive_meta()?;
                }
                db.checkpoint()?;
                // Footer catalog is always xz -9e, independent of tar stream compression.
                match archive_footer::write_footer(
                    &config.paths.archive_path,
                    &config.paths.db_path()) {
                    Ok(()) => (),
                    Err(e) => {
                        recorder.record_session(
                            ERROR_PHASE,
                            e.to_file_stat(Some(&config.paths.db_path())),
                            ErrorFlags::default(),
                        );
                        return Err(e);
                    }
                }
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
