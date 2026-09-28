use std::fs::OpenOptions;
use std::path::Path;
use std::{fs, io};

use crate::archive::ArchiveRTArgs;
use crate::archive_footer;
use crate::common::files::warn_if_times_changed;
use crate::common::{SNAPSHOT_INIT_TAR_NAME, SNAPSHOT_TAR_NAME};
use crate::db::ErrorPhase;
use crate::db::flags::{ErrorFlags, FileFlag};
use crate::db::types::{FilePhase, StrippedRecord};
use crate::db::{Database, Recorder};
use crate::error::{Error, FileStatError, Result};
use crate::tar_writer::TarWriter;

const ERROR_PHASE: ErrorPhase = ErrorPhase::Pipeline(crate::config::PipelinePhase::Archive);

// INFO - Archival works as follows:
//  Ineligible files are promoted first.

pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    let mut recorder = Recorder::new(rt.db, !rt.config.process.no_errors);
    // Crash / force leftover: truncate incomplete stream C, keep finished A..B.
    recover_incomplete_session(rt, &mut recorder)?;

    let archive_offset = archive_file_len(&rt.config.paths.archive_path);
    check_archive_bytes_out(rt.db, archive_offset)?;

    debug_assert!(rt.db.open_archive_session()?.is_none());
    let session_id = rt.db.begin_archive_session(archive_offset)?;

    // Require sha1 unless retry_missing_sha asks to include unhashed files.
    rt.db.promote_ineligible_to_archived()?;
    let promoted = rt.db.count_files_in_phase(FilePhase::Archived)?;
    rt.progress.inc_global(promoted);

    let bytes_in_base = rt.db.get_archive_bytes_in()?;
    let total_bytes = rt.db.sum_canonical_bytes_to_archive()?;
    let already_archived = rt.db.sum_archived_canonical_bytes()?;

    // TODO update eta only when write to buff occurs.
    let progress = rt.progress;
    progress.set_phase_total(total_bytes);
    progress.set_phase_position(already_archived);

    let mut writer = TarWriter::open(
        rt.config.paths.archive_path.clone(),
        &rt.config.compression,
        rt.config.process.jobs,
        rt.shutdown.clone(),
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
    let to_archive = rt.db.list_staged_canonical_ordered()?;
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
        if rt.shutdown.is_graceful() {
            stopped = true;
            final_archive = false;
            break;
        }

        let record = rt.db.get_file_by_id::<StrippedRecord>(file_id)?.expect(
            "File was present in db for listing; missing row means SQL/list bug or DB corruption",
        );
        let tar_name = record.tar_member_name().expect(
            "INVARIANT ERROR: Members to be encoded must have a symlink in the \
            staging directory.",
        );
        let source = rt.config.paths.stage_dir().join(&tar_name);

        // Stage path is a symlink; compare inventory times against the real target.
        let target = match fs::canonicalize(&source) {
            Ok(t) => t,
            Err(e) => {
                capture_error(Error::copy_io(&source, &e), &mut recorder, file_id);
                rt.db.set_file_flag(record.id, FileFlag::ErrorWhileArchive, true)?;
                if rt.config.process.fail_fast {
                    return Err(Error::io(&source, e));
                }
                continue;
            }
        };
        let modified = warn_if_times_changed(
            &target, record.mtime, record.atime, record.ctime
        );

        progress.set_phase_file("archive", &record.abs_path);
        match writer.append_path(
            &source, &tar_name, |n| progress.inc_phase(n)) {
            Ok(()) => {
                rt.db.set_file_flag(record.id, FileFlag::AppendedPath, true)?;
                if modified {
                    rt.db.set_file_flag(record.id, FileFlag::Modified, true)?;
                }
                progress.inc_global(1);
            }
            Err(e @ Error::FileStat(_)) => {
                tracing::error!(
                    path = %record.abs_path.display(),
                    error = %e,
                    "archive append_path failed; marking ErrorWhileArchive and continuing"
                );
                capture_error(e, &mut recorder, file_id);
                rt.db.set_file_flag(record.id, FileFlag::ErrorWhileArchive, true)?;
                // Do not set AppendedPath — member was not successfully written.
            }
            Err(e) => {
                panic!("Unexpected return type {e}, only FileStatError Expected");
            }
        }
    }

    // Fast exit on force - abort now.
    if stopped && rt.shutdown.is_force() {
        return force_abort_session(rt, writer);
    }


    // PRECONDITION: Any (force) aborts are not treated here anymore.else
    tracing::info!("Finishing up session...");
    end_session(
        writer,
        rt,
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

/// Look for incomplete session. If found -> Archive was not closed properly and cannot be extended.
/// The incomplete session will be undone by truncating the archive. So if an incomplet session
/// is found:
/// - Truncate the archive to the previous session - if any,
/// - Update db to mark the incomplete session as aborted.
/// POSTCONDITION: The archive (if present) is ready to be concatenated with a new archive.
fn recover_incomplete_session(
    rt: &ArchiveRTArgs,
    recorder: &mut Recorder)
    -> Result<()> {
    let open_session = match rt.db.open_archive_session()? {
        None => return Ok(()),
        Some(s) => s,
    };

    let res = truncate_archive_at(
        &rt.config.paths.archive_path,
        open_session.archive_offset
    ).map_err(|e| FileStatError::io(&rt.config.paths.archive_path, e));

    match res {
        Ok(_) => (),
        Err(e) => {
            recorder.record_session(ERROR_PHASE, e.recreate(), ErrorFlags::default());
            return Err(Error::FileStat(e));
        }
    }

    rt.db.abort_incomplete_archive_session(&open_session)?;

    tracing::info!(
        "recovered incomplete archive session at offset {} ({})",
        open_session.archive_offset,
        rt.config.paths.archive_path.display()
    );
    Ok(())
}

/// Force abort: abandon the writer in place. Leave session `finalized = 0` and
/// pending file flags; next run's startup recovery truncates and marks aborted.
fn force_abort_session(rt: &ArchiveRTArgs, writer: TarWriter) -> Result<()> {
    writer.abandon();
    // Ensure pending flags + open session are durable before exit.
    rt.db.checkpoint()?;
    rt.progress.abandon();
    Err(Error::Interrupted)
}

/// append_snapshot, commits the db, stages it, adds it to the archive and removes the stage again.
fn append_snapshot(
    writer: &mut TarWriter,
    rt: &ArchiveRTArgs,
    is_init: bool,
    recorder: &mut Recorder)
    -> Result<()> {

    rt.db.checkpoint()?;
    let src = rt.config.paths.db_path();
    let staging_target = rt.config.paths.stage_archive_snapshot();
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
    match fs::copy(&src, &staging_target) {
        Ok(_) => (),
        Err(e) => {
            capture_error(&e, &staging_target);
            return Err(Error::io(&staging_target, e));
        }
    }
    let tar_dst = if is_init { SNAPSHOT_INIT_TAR_NAME } else { SNAPSHOT_TAR_NAME };
    let result = writer
        .append_path(&staging_target, tar_dst, |_| ());
    let _ = fs::remove_file(&staging_target).map_err(|e| capture_error(&e, &staging_target));
    result
}

fn end_session(
    mut writer: TarWriter,
    rt: &ArchiveRTArgs,
    session_id: i64,
    bytes_in_base: u64,
    write_tar_eof: bool,
    recorder: &mut Recorder)
    -> Result<()> {

    // Finish session by promoting to 'archived'
    let n_pending = rt.db.promote_pending_archived()?;
    rt.progress.inc_global(n_pending);

    // Full archive pass only: sanity check we have nothing left.
    if write_tar_eof {
        assert_eq!(0, rt.db.count_files_in_phase(FilePhase::Staged)?,
                   "INVARIANT ERROR: Files remaining while attempting to finalize the session.");
    }
    rt.db.stamp_archive_session_finished_at(session_id)?;

    rt.progress.set_phase_msg(&format!("archive writing {SNAPSHOT_TAR_NAME} (progress)"));
    // Write session / archive final snapshot.
    append_snapshot(&mut writer, rt, false, recorder)?;

    rt.progress.set_phase_msg("archive finalizing compression stream");
    let result = if write_tar_eof {
        // ARCHIVE!!!
        writer.finalize_archive()
    } else {
        // SESSION!!!
        writer.finalize_session()
    };

    match result {
        Ok((session_bytes_in, bytes_out)) => {
            rt.db.finalize_archive_session(session_id)?;
            rt.db.set_archive_bytes_in(bytes_in_base.saturating_add(session_bytes_in))?;
            rt.db.set_archive_bytes_out(bytes_out)?;

            if write_tar_eof && rt.config.pipeline.write_archive_footer {
                if rt.config.pipeline.clear_archive_meta {
                    rt.db.clear_archive_meta()?;
                }
                rt.db.checkpoint()?;
                // Footer catalog is always xz -9e, independent of tar stream compression.
                match archive_footer::write_footer(
                    &rt.config.paths.archive_path,
                    &rt.config.paths.db_path()) {
                    Ok(()) => (),
                    Err(e) => {
                        recorder.record_session(
                            ERROR_PHASE,
                            e.to_file_stat(Some(&rt.config.paths.db_path())),
                            ErrorFlags::default(),
                        );
                        return Err(e);
                    }
                }
            }
            Ok(())
        }
        Err(Error::Interrupted) if rt.shutdown.is_force() => {
            panic!("Interrupt should not be raised here.")
        }
        Err(e) => Err(e),
    }
}

// -------------------------------------------------------------------------------------------------
// UTIL
// -------------------------------------------------------------------------------------------------

/// Truncate archive to `offset` (end of previous finished stream / start of incomplete one).
fn truncate_archive_at(path: &Path, offset: u64) -> io::Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(offset)?;
    file.sync_all()? ;
    if offset == 0 {
        // Empty archive file: remove so next session starts clean.
        drop(file);
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Get file len of archive, default to 0 even if file does not exist.
fn archive_file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// After recovery: if a prior session finalized cleanly, archive length must match meta.
/// INFO: The meta fields are only set on successful exit.
fn check_archive_bytes_out(db: &Database, archive_len: u64) -> Result<()> {
    if !db.has_finalized_archive_session()? {
        return Ok(());
    }
    let Some(expected) = db.get_archive_bytes_out()? else {
        return Ok(());
    };
    assert_ne!(archive_len, expected,
        "archive file length {archive_len} does not match recorded archive_bytes_out {expected} \
         (file truncated or modified externally)"
    );
    Ok(())
}