use std::fs::OpenOptions;
use std::path::Path;
use std::{fs, io};

use crate::archive::ArchiveRTArgs;
use crate::archive_footer;
use crate::common::defaults::{SNAPSHOT_INIT_TAR_NAME, SNAPSHOT_TAR_NAME};
use crate::common::files::{read_probe, warn_if_times_changed};
use crate::common::{batched_stepped_loop, io_buffer};
use crate::config::PipelinePhase;
use crate::db::ErrorPhase;
use crate::db::flags::{ErrorFlags, FileFlag};
use crate::db::types::{FilePhase, StrippedRecord};
use crate::db::{Database, Recorder};
use crate::error::{Error, FileStatError, Result};
use crate::progress::BarKind;
use crate::tar_writer::TarWriter;

const ERROR_PHASE: ErrorPhase = ErrorPhase::Pipeline(crate::config::PipelinePhase::Archive);

// INFO - Archival works as follows:
//  Ineligible files are promoted first.
//  Then, the eligible files are appended to the archive one by one.
//  If it fails, the ErrorWhileArchive is set
//  If it succeeds the append records the archive session_id
//  If a session is force aborted, the stream is abandoned and the database committed
//  If a session is exiting gracefully or was not interrupted, the set the phase of the files with
//  the ArchivedPath to 'archived'. Then we append a snapshot of the database into the stream prior
//  to finalizing and closing the stream.
//  (??? Files with ErrorWhileArchive need to be promoted to 'archived')

pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    // Bypass archive: finalize a directory product (canonical payload files +
    // `tar-dedup.sqlite`) instead of a compressed tar stream. No stream, no
    // footer, no sha1 — see `run_bypass_archive`.
    if rt.config.pipeline.bypass_archive {
        return run_bypass_archive(rt);
    }

    let mut recorder = Recorder::new(rt.db, !rt.config.process.no_errors, Some(rt.config.process.auto_flush_limit));
    // Crash / force leftover: truncate incomplete stream C, keep finished A.B.
    recover_incomplete_session(rt, &mut recorder)?;

    // Handle writing footer in case compression stream is finished and footer interrupted during
    // hash.
    // PRECONDITION: No open session.
    if footer_pending(rt)? {
        tracing::info!("archive finalized but footer missing; completing the trailer");
        run_footer_one_shot(rt)?;
        return Ok(());
    }

    let archive_offset = archive_file_len(&rt.config.paths.archive_path);
    check_archive_bytes_out(rt.db, archive_offset)?;

    debug_assert!(rt.db.open_tar_writer_session()?.is_none());
    let session_id = rt.db.begin_tar_writer_session(archive_offset)?;

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
        rt.config.sparse.sparsify,
    )?;

    // Fresh start into archiving.
    if already_archived == 0 {
        progress.set_phase_msg(&format!(
            "archive writing {SNAPSHOT_INIT_TAR_NAME} (initial manifest)"
        ));
        append_snapshot(&mut writer, rt, true, &mut recorder)?;
    }

    // Batched queue of staged canonicals in packing order; the ordering table
    // survives interrupts so a resume repopulate is a no-op for queued rows.
    rt.db.create_archive_queue()?;
    rt.db.populate_archive_queue(rt.config.pipeline.order)?;
    let to_archive = rt.db.count_files_in_phase(FilePhase::Staged)?;
    if to_archive == 0 && already_archived == 0 {
        tracing::warn!("no staged files to archive");
    }

    tracing::info!(
        "starting archive: total {}, processing {}, pending {} bytes",
        rt.db.count_entries()?,
        to_archive,
        total_bytes.saturating_sub(already_archived),
    );

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

    // Process the queue in batches. `pull_pending_archive_rows` advances the read
    // cursor by the last queue position of each batch; already-appended /
    // errored rows exit the pull until the next session.
    match batched_stepped_loop(
        rt.config.process.batch_size,
        || 0u64,
        |index: &u64, batch_size| {
            rt.db.pull_pending_archive_rows::<StrippedRecord>(*index, batch_size)
        },
        |(pos, _rec): &(u64, StrippedRecord)| *pos,
        |batch: Vec<(u64, StrippedRecord)>| {
            for (_pos, record) in batch {
                // Break on any interrupt: graceful finalizes the session, force
                // abandons the stream (see below).
                if rt.shutdown.is_interrupted() {
                    stopped = true;
                    final_archive = false;
                    return Err(Error::Interrupted);
                }

                let tar_name = record.tar_member_name().expect(
                    "INVARIANT ERROR: Members to be encoded must have a symlink in the \
                    staging directory.",
                );
                let source = rt.config.paths.stage_dir().join(&tar_name);

                // Stage path is a symlink; compare inventory times against the real target.
                let target = match fs::canonicalize(&source) {
                    Ok(t) => t,
                    Err(e) => {
                        capture_error(Error::copy_io(&source, &e), &mut recorder, record.id);
                        rt.db.set_file_flag(record.id, FileFlag::ErrorWhileArchive, true)?;
                        rt.db.set_file_phase(record.id, FilePhase::Archived)?;
                        if rt.config.process.fail_fast {
                            return Err(Error::io(&source, e));
                        }
                        continue;
                    }
                };
                let times = warn_if_times_changed(
                    &target, record.mtime, record.atime, record.ctime
                );

                progress.set_phase_file("archive", &record.abs_path);
                match writer.append_path(
                    &source, &tar_name, |n| progress.inc_phase(n)) {
                    Ok(()) => {
                        tracing::debug!("appended {} ({} bytes)", tar_name, record.size);
                        rt.db.mark_tar_writer_session_pending(record.id, session_id)?;
                        rt.db.or_file_flags(record.id, times.to_file_flags())?;
                        progress.inc_global(1);
                    }
                    Err(e @ Error::FileStat(_)) => {
                        tracing::error!(
                            path = %record.abs_path.display(),
                            error = %e,
                            "archive append_path failed; marking ErrorWhileArchive and continuing"
                        );
                        capture_error(e, &mut recorder, record.id);
                        rt.db.set_file_flag(record.id, FileFlag::ErrorWhileArchive, true)?;
                        rt.db.set_file_phase(record.id, FilePhase::Archived)?;
                        // Do not mark the archive session — the member was not written.
                    }
                    Err(Error::Interrupted) => {
                        // Force abort surfaced mid-member; stop the batch loop now.
                        stopped = true;
                        final_archive = false;
                        return Err(Error::Interrupted);
                    }
                    Err(e) => {
                        panic!("Unexpected return type {e}, only FileStatError Expected");
                    }
                }
            }
            Ok(())
        }) {
        Ok(()) => (),
        // Control-flow for an interrupt detected above; the `stopped` flags
        // decide graceful vs force in the post-loop section.
        Err(Error::Interrupted) => (),
        Err(e) => return Err(e),
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

    rt.db.drop_archive_queue()?;
    recorder.flush()?;
    let done_bytes = rt.db.sum_archived_canonical_bytes()?;
    tracing::info!(
        "archive done: processed {} of {} bytes, {} error(s)",
        done_bytes,
        total_bytes,
        rt.db.count_errors(Some("archive"))?,
    );
    Ok(())
}

/// Record a per-file archive-phase failure against the persistent error log.
fn record_file_error(recorder: &mut Recorder, fid: crate::db::types::FileId, path: &Path, e: &io::Error) {
    recorder.record_file(
        fid, ERROR_PHASE, FileStatError::copy_io(path, e), ErrorFlags::default(),
    );
}

/// Finalize the bypass-archive directory product: run the canonical payloads
/// (resolving the stage symlinks) into the product dir — `-f DIR` — then bake a
/// checkpointed `tar-dedup.sqlite` beside them. After the run closes the DB, the
/// caller promotes that file to the immutable `manifest.sqlite`. Uses the same
/// session machinery
/// as the tar path (begin session, queue pull, `mark_archive_session_pending`,
/// promote, `finalize_archive_session`), so a resume/--force behaves identically
/// and the A7 append invariant still holds. There is no compression stream, no
/// footer and no stream sha1: content rehash is the integrity guarantee on the
/// extract side (`--force` opts out).
fn run_bypass_archive(rt: &ArchiveRTArgs) -> Result<()> {
    // Mark the product provenance before baking the catalog so the copy carries it.
    rt.db.set_bypass_archive(true)?;
    let mut recorder = Recorder::new(
        rt.db, !rt.config.process.no_errors, Some(rt.config.process.auto_flush_limit));
    let mut buf = io_buffer();

    // A leftover open session means a prior bypass finalize was interrupted:
    // the work-db still holds the truth, the product dir is partial. Truncating a
    // stream is meaningless here, so just drop the stale session state and retry.
    if let Some(open) = rt.db.open_tar_writer_session()? {
        rt.db.abort_incomplete_tar_writer_session(&open)?;
        tracing::warn!(
            "recovered incomplete bypass-archive session ({})",
            rt.config.paths.stage_dir().display()
        );
    }

    let session_id = rt.db.begin_tar_writer_session(0)?;

    rt.db.promote_ineligible_to_archived()?;
    let promoted = rt.db.count_files_in_phase(FilePhase::Archived)?;
    rt.progress.inc_global(promoted);

    let total_bytes = rt.db.sum_canonical_bytes_to_archive()?;
    let already_archived = rt.db.sum_archived_canonical_bytes()?;
    rt.progress.set_phase_total(total_bytes);
    rt.progress.set_phase_position(already_archived);

    rt.db.create_archive_queue()?;
    rt.db.populate_archive_queue(rt.config.pipeline.order)?;
    let to_archive = rt.db.count_files_in_phase(FilePhase::Staged)?;
    if to_archive == 0 && already_archived == 0 {
        tracing::warn!("no staged files to archive");
    }
    tracing::info!(
        "starting bypass-archive: total {}, processing {}, pending {} bytes",
        rt.db.count_entries()?,
        to_archive,
        total_bytes.saturating_sub(already_archived),
    );

    match batched_stepped_loop(
        rt.config.process.batch_size,
        || 0u64,
        |index: &u64, batch_size| {
            rt.db.pull_pending_archive_rows::<StrippedRecord>(*index, batch_size)
        },
        |(pos, _rec): &(u64, StrippedRecord)| *pos,
        |batch: Vec<(u64, StrippedRecord)>| {
            for (_pos, record) in batch {
                if rt.shutdown.is_interrupted() {
                    return Err(Error::Interrupted);
                }

                let cid_name = record.tar_member_name().expect(
                    "INVARIANT ERROR: Members to be encoded must have a symlink in the \
                    staging directory.",
                );
                let stage_src = rt.config.paths.stage_dir().join(&cid_name);
                // Stage path is a symlink; resolve for the times warning and the
                // underlying bytes are what the product needs.
                let target = match fs::canonicalize(&stage_src) {
                    Ok(t) => t,
                    Err(e) => {
                        record_file_error(&mut recorder, record.id, &stage_src, &e);
                        rt.db.set_file_flag(record.id, FileFlag::ErrorWhileArchive, true)?;
                        if rt.config.process.fail_fast {
                            return Err(Error::io(&stage_src, e));
                        }
                        continue;
                    }
                };
                let times = match !record.flags.get(FileFlag::HasSparse) {
                    true => None,
                    false => Some(warn_if_times_changed(
                        &target, record.mtime, record.atime, record.ctime,
                    ))
                };

                rt.progress.set_phase_file("archive", &record.abs_path); // TODO check this.
                match read_probe(&target, record.size, &mut buf) {
                    Ok(_) => {
                        rt.db.mark_tar_writer_session_pending(record.id, session_id)?;
                        if let Some(times) = times {
                            rt.db.or_file_flags(record.id, times.to_file_flags())?;
                        }
                        rt.progress.inc_global(1);
                    }
                    Err(e) => {
                        record_file_error(&mut recorder, record.id, &target, &e);
                        rt.db.set_file_flag(record.id, FileFlag::ErrorWhileArchive, true)?;
                    }
                }
            }
            Ok(())
        }) {
        Ok(()) => (),
        Err(Error::Interrupted) => {
            // Graceful/force interrupt: leave the session open (recovery handles
            // it next run) and report the interrupt.
            progress_abort_report(rt);
            return Err(Error::Interrupted);
        }
        Err(e) => return Err(e),
    }

    // Promote the session's appended rows to `archived`, then finalize.
    let n_pending = rt.db.promote_pending_to_archived()?;
    rt.progress.inc_global(n_pending);
    assert_eq!(
        0,
        rt.db.count_files_in_phase(FilePhase::Staged)?,
        "INVARIANT ERROR: Files remaining while attempting to finalize the bypass-archive session."
    );
    rt.db.stamp_tar_writer_session_finished_at(session_id)?;
    rt.db.finalize_tar_writer_session(session_id)?;

    // Bake the finalized work-db into the product dir; `archive::run` renames it
    // to `manifest.sqlite` after the DB closes.
    rt.db.drop_archive_queue()?;
    recorder.flush()?;
    rt.db.checkpoint()?;

    let done_bytes = rt.db.sum_archived_canonical_bytes()?;
    tracing::info!(
        "bypass-archive done: processed {} of {} bytes, {} error(s); product in {}",
        done_bytes,
        total_bytes,
        rt.db.count_errors(Some("archive"))?,
        rt.config.paths.stage_dir().display(),
    );
    Ok(())
}

/// Report an interrupted bypass finalize and ensure the in-flight state is durable.
#[cold]
fn progress_abort_report(rt: &ArchiveRTArgs) {
    rt.db.checkpoint().ok();
    rt.progress.abandon();
    tracing::warn!(
        "bypass-archive interrupted during finalize; rerun to resume (staged rows re-copied)"
    );
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
    let open_session = match rt.db.open_tar_writer_session()? {
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

    rt.db.abort_incomplete_tar_writer_session(&open_session)?;

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
    let n_pending = rt.db.promote_pending_to_archived()?;
    rt.progress.inc_global(n_pending);

    // Full archive pass only: sanity check we have nothing left.
    if write_tar_eof {
        assert_eq!(0, rt.db.count_files_in_phase(FilePhase::Staged)?,
                   "INVARIANT ERROR: Files remaining while attempting to finalize the session.");
    }
    rt.db.stamp_tar_writer_session_finished_at(session_id)?;

    rt.progress.set_phase_msg(&format!("archive writing {SNAPSHOT_TAR_NAME} (progress)"));
    // Write session / archive final snapshot.
    append_snapshot(&mut writer, rt, false, recorder)?;

    rt.progress.set_phase_msg("archive finalizing compression stream");
    // `write_tar_eof` = final archive close (commit EOF); false = graceful session end.
    let result = writer.finalize(write_tar_eof);

    match result {
        Ok((session_bytes_in, bytes_out)) => {
            rt.db.finalize_tar_writer_session(session_id)?;
            rt.db.set_archive_bytes_in(bytes_in_base.saturating_add(session_bytes_in))?;
            rt.db.set_archive_bytes_out(bytes_out)?;

            if write_tar_eof && rt.config.pipeline.write_archive_footer {
                rt.db.checkpoint()?;
                // Footer catalog is always xz -9e, independent of tar stream compression.
                // The content-integrity hash reads the finished archive back; drop the
                // phase/global bars so the trailer hash is the only bar shown.
                rt.progress.single_bar();
                let hash_bar = rt.progress.push_sub_bar("archive integrity: ", BarKind::Bytes);

                let mut eager_state = rt.db.load_archive_runtime_state()?
                    .expect("RT State set after every phase. Must re present");
                eager_state.phase = PipelinePhase::Done;
                rt.db.save_archive_runtime_state(&eager_state)?;

                match archive_footer::write_footer(
                    &rt.config.paths.archive_path,
                    &rt.config.paths.db_path(),
                    rt.shutdown,
                    Some(&hash_bar)) {
                    Ok(()) => (),
                    Err(e @ Error::Interrupted) => return Err(e),
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
        Err(e) => Err(e), // (Expected to be Io, Config, DB)
    }
}

/// True when the archive is a complete, finalized stream missing only its
/// trailer: footer enabled, no valid footer yet, the session finalized, and
/// every row `archived`. Both the finalized guard and the full promotion guard
/// are required: a graceful mid-archive interrupt finalizes the session but
/// leaves un-appended rows `staged` (so `count(Archived) == count_entries` fails),
/// while a crash between `promote_pending_archived` and `finalize_archive_session`
/// leaves every row `archived` with the session still open (so the finalized
/// guard fails). Only the true fork is the interrupted-footer-hash state.
fn footer_pending(rt: &ArchiveRTArgs) -> Result<bool> {
    Ok(rt.config.pipeline.write_archive_footer
        && !archive_footer::has_valid_footer(&rt.config.paths.archive_path)
        && rt.db.has_finalized_archive_session()?
        && rt.db.count_files_in_phase(FilePhase::Archived)? == rt.db.count_entries()?)
}

/// One-shot completion of a footerless-but-finalized archive (see the resume
/// branch in `run`): re-open the finalized work DB, hash the stream bytes and
/// append the trailer. Runs before any new session / re-compression.
fn run_footer_one_shot(rt: &ArchiveRTArgs) -> Result<()> {
    // Drop the phase/global bars so the trailer hash is the only bar shown.
    rt.progress.single_bar();
    let hash_bar = rt.progress.push_sub_bar("archive integrity: ", BarKind::Bytes);
    archive_footer::write_footer(
        &rt.config.paths.archive_path,
        &rt.config.paths.db_path(),
        rt.shutdown,
        Some(&hash_bar))
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
        fs::remove_file(path)?;
    }
    Ok(())
}

/// Get file len of archive, default to 0 even if file does not exist.
fn archive_file_len(path: &Path) -> u64 {
    fs::metadata(path).map(|m| m.len()).unwrap_or(0)
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
    assert_eq!(archive_len, expected,
        "archive file length {archive_len} does not match recorded archive_bytes_out {expected} \
         (file truncated or modified externally)"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::archive_footer;
    use crate::db::flags::{ErrorScope, FileFlag};
    use crate::db::types::FileId;
    use crate::shutdown::Shutdown;
    use crate::test_support::TestWorld;
    use chrono::{DateTime, Utc};
    use nix::unistd::geteuid;
    use rusqlite::{OptionalExtension, named_params};
    use std::os::unix::fs::PermissionsExt;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    impl TestWorld {
        /// Mark `id` canonical + already archived (prior session). Counts toward
        /// `already_archived`, which suppresses the init-manifest write in run().
        fn seed_archived_row(&self, id: FileId) {
            self.db.with_transaction(|conn| {
                let n = conn.execute(
                    "UPDATE files SET phase = 'archived', canonical_id = :id, \
                     sha1 = :sha1, include_reason_archive = -1, exclude_reason_archive = 0 \
                     WHERE id = :id",
                    named_params! {
                        ":id": id.0,
                        ":sha1": [7u8; 20].as_slice(),
                    },
                ).expect("seed archived row");
                assert_eq!(n, 1);
                Ok(())
            }).expect("seed archived row tx");
        }

        fn flags(&self, id: FileId) -> crate::db::flags::FileFlags {
            self.db.get_file_flags(id).expect("get flags")
        }

        /// Open a session row (open = finalized 0) and return its id.
        fn begin_session(&self, offset: u64) -> i64 {
            self.db.begin_tar_writer_session(offset).expect("begin session")
        }

        fn session_finalized(&self, session_id: i64) -> i64 {
            self.db.with_transaction(|conn| {
                let f: i64 = conn.query_row(
                    "SELECT finalized FROM archive_sessions WHERE id = :id",
                    named_params! { ":id": session_id },
                    |row| row.get(0),
                ).expect("read session finalized");
                Ok(f)
            }).expect("session finalized tx")
        }

        /// The archive session this row's payload was appended in (None = not yet).
        fn session_of(&self, id: FileId) -> Option<i64> {
            self.db.with_transaction(|conn| {
                let s: Option<i64> = conn
                    .query_row("SELECT session_id FROM files WHERE id = :id",
                        named_params! { ":id": id.0 }, |row| row.get(0))
                    .optional().expect("read session").and_then(|s| s);
                Ok(s)
            }).expect("session tx")
        }
    }

    // -------------------------------------------------------------------------
    // Util
    // -------------------------------------------------------------------------

    #[test]
    fn archive_file_len_missing_is_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(archive_file_len(&dir.path().join("nope.astar")), 0);
    }

    #[test]
    fn archive_file_len_returns_len() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("a.astar");
        fs::write(&p, vec![0u8; 1234]).expect("write");
        assert_eq!(archive_file_len(&p), 1234);
    }

    #[test]
    fn truncate_archive_at_nonexistent_is_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("nope.astar");
        truncate_archive_at(&p, 10).expect("truncate missing is no-op");
        assert!(!p.exists());
    }

    #[test]
    fn truncate_archive_at_truncates_to_offset() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("t.astar");
        fs::write(&p, vec![0u8; 1_048_576]).expect("write");
        truncate_archive_at(&p, 4096).expect("truncate");
        assert_eq!(fs::metadata(&p).expect("meta").len(), 4096);
        assert!(p.exists());
    }

    #[test]
    fn truncate_archive_at_zero_removes_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let p = dir.path().join("z.astar");
        fs::write(&p, vec![0u8; 32]).expect("write");
        truncate_archive_at(&p, 0).expect("truncate to zero");
        assert!(!p.exists());
    }

    #[test]
    fn check_bytes_out_ignored_without_finalized_session() {
        let world = TestWorld::new();
        check_archive_bytes_out(&world.db, 999).expect("no session -> ok");
    }

    #[test]
    fn check_bytes_out_ok_when_meta_absent() {
        let world = TestWorld::new();
        let sid = world.begin_session(0);
        world.db.finalize_tar_writer_session(sid).expect("finalize");
        // bytes_out meta never recorded -> skip
        check_archive_bytes_out(&world.db, 999).expect("absent meta -> ok");
    }

    #[test]
    fn check_bytes_out_ok_when_equal() {
        let world = TestWorld::new();
        let sid = world.begin_session(0);
        world.db.finalize_tar_writer_session(sid).expect("finalize");
        world.db.set_archive_bytes_out(42).expect("set bytes out");
        check_archive_bytes_out(&world.db, 42).expect("equal -> ok");
    }

    #[test]
    fn check_bytes_out_panics_on_mismatch() {
        let world = TestWorld::new();
        let sid = world.begin_session(0);
        world.db.finalize_tar_writer_session(sid).expect("finalize");
        world.db.set_archive_bytes_out(42).expect("set bytes out");
        let res = catch_unwind(AssertUnwindSafe(|| {
            check_archive_bytes_out(&world.db, 43).expect("unequal -> panic")
        }));
        assert!(res.is_err(), "length mismatch must panic");
    }

    // -------------------------------------------------------------------------
    // Session / recovery
    // -------------------------------------------------------------------------

    #[test]
    fn recover_incomplete_session_without_open_session_is_ok() {
        let world = TestWorld::new();
        let mut recorder = Recorder::new(&world.db, true, None);
        recover_incomplete_session(&world.rt(), &mut recorder).expect("no session -> ok");
    }

    #[test]
    fn recover_incomplete_session_truncate_error_aborts() {
        let world = TestWorld::new();
        let mut recorder = Recorder::new(&world.db, true, None);
        world.begin_session(100);
        // Archive path is a directory: write-open fails, truncate errors out.
        let dir_path = world.dir.path().join("blocker");
        fs::create_dir_all(&dir_path).expect("create dir");
        let mut config = world.config.clone();
        config.paths.archive_path = dir_path.clone();
        let rt = ArchiveRTArgs {
            config: &config,
            db: &world.db,
            shutdown: &world.shutdown,
            progress: &world.progress,
        };

        let res = recover_incomplete_session(&rt, &mut recorder);
        match res {
            Err(Error::FileStat(fse)) => {
                assert_eq!(fse.io_path(), Some(dir_path));
            }
            other => panic!("expected FileStat, got {other:?}"),
        }
        let _ = recorder.flush();
        // A session-scoped error row was recorded.
        let n = world.db.count_records(ErrorScope::from_bits(1 << 2), None).expect("count");
        assert!(n >= 1, "expected a session-scoped error record, got {n}");
    }

    #[test]
    fn force_abort_session_leaves_session_open_no_bytes() {
        let world = TestWorld::new();
        let id = world.add_file("a.bin", &[1u8; 8][..]);
        world.seed_staged_row(id);
        world.place_symlink(id);
        let sid = world.begin_session(0);

        let mut recorder = Recorder::new(&world.db, true, None);
        let writer = TarWriter::open(
            world.config.paths.archive_path.clone(),
            &world.config.compression,
            world.config.process.jobs,
            world.shutdown.clone(),
            true,
        ).expect("open writer");

        let res = force_abort_session(&world.rt(), writer);
        assert!(matches!(res, Err(Error::Interrupted)));
        let _ = recorder.flush();

        // Session stays open; no bytes meta; nothing promoted.
        assert_eq!(world.session_finalized(sid), 0);
        assert!(world.db.get_archive_bytes_out().expect("bytes out").is_none());
        assert_eq!(world.db.get_archive_bytes_in().expect("bytes in"), 0);
        assert_eq!(world.phase(id), FilePhase::Staged);
        assert!(world.session_of(id).is_none(), "force abort must not record a session");
    }

    #[test]
    fn append_snapshot_uses_init_vs_regular_name() {
        let world = TestWorld::new();
        let mut recorder = Recorder::new(&world.db, true, None);

        let mut writer = TarWriter::open(
            world.config.paths.archive_path.clone(),
            &world.config.compression,
            world.config.process.jobs,
            world.shutdown.clone(),
            true,
        ).expect("open writer");
        append_snapshot(&mut writer, &world.rt(), true, &mut recorder).expect("init snapshot");
        writer.finalize(true).expect("finalize init");

        let raw = fs::read(&world.config.paths.archive_path).expect("read archive");
        assert!(raw.windows(SNAPSHOT_INIT_TAR_NAME.len())
            .any(|w| w == SNAPSHOT_INIT_TAR_NAME.as_bytes()));
    }

    #[test]
    fn append_snapshot_staging_file_removed_after() {
        let world = TestWorld::new();
        let mut recorder = Recorder::new(&world.db, true, None);

        let mut writer = TarWriter::open(
            world.config.paths.archive_path.clone(),
            &world.config.compression,
            world.config.process.jobs,
            world.shutdown.clone(),
            true,
        ).expect("open writer");
        append_snapshot(&mut writer, &world.rt(), false, &mut recorder).expect("snapshot");
        let staging = world.config.paths.stage_archive_snapshot();
        assert!(!staging.exists(), "staging copy must be removed after append");
        writer.finalize(true).expect("finalize");
    }

    #[test]
    fn append_snapshot_graceful_does_not_abort() {
        let world = TestWorld::new();
        world.shutdown.request_graceful();
        let mut recorder = Recorder::new(&world.db, true, None);

        let mut writer = TarWriter::open(
            world.config.paths.archive_path.clone(),
            &world.config.compression,
            world.config.process.jobs,
            world.shutdown.clone(),
            true,
        ).expect("open writer");
        // Snapshot is the progress checkpoint; a graceful request must not kill
        // it (check_in_flight between files is where interrupts land).
        append_snapshot(&mut writer, &world.rt(), false, &mut recorder).expect("still appends");
        writer.finalize(true).expect("finalize");
    }

    #[test]
    fn end_session_promotes_pending_to_archived() {
        let world = TestWorld::new();
        let id = world.add_file("a.bin", &[1u8; 8][..]);
        world.seed_staged_row(id);
        world.place_symlink(id);
        let sid = world.begin_session(0);
        world.db.mark_tar_writer_session_pending(id, sid).expect("mark appended");

        let writer = TarWriter::open(
            world.config.paths.archive_path.clone(),
            &world.config.compression,
            world.config.process.jobs,
            world.shutdown.clone(),
            true,
        ).expect("open writer");
        let mut recorder = Recorder::new(&world.db, true, None);
        end_session(writer, &world.rt(), sid, 0, false, &mut recorder).expect("end session");

        assert_eq!(world.phase(id), FilePhase::Archived);
        assert_eq!(world.session_finalized(sid), 1);
    }

    #[test]
    fn end_session_panics_when_staged_remain_on_full_close() {
        let world = TestWorld::new();
        let id = world.add_file("a.bin", &[1u8; 8][..]);
        world.seed_staged_row(id);
        world.place_symlink(id);
        // No session_id: promote_pending leaves it staged -> invariant panic.
        let sid = world.begin_session(0);

        let writer = TarWriter::open(
            world.config.paths.archive_path.clone(),
            &world.config.compression,
            world.config.process.jobs,
            world.shutdown.clone(),
            true,
        ).expect("open writer");
        let mut recorder = Recorder::new(&world.db, true, None);
        let res = catch_unwind(AssertUnwindSafe(|| {
            end_session(writer, &world.rt(), sid, 0, true, &mut recorder).expect("end session")
        }));
        assert!(res.is_err(), "full-archive close with staged rows must panic");
    }

    #[test]
    fn end_session_writes_footer_when_configured() {
        let world = TestWorld::new();
        // The archive phase loop saves the runtime state before tar_builder runs;
        // this test calls `end_session` directly, so seed it (phase Done).
        crate::test_support::seed_archive_done(&world.db);
        let mut config = world.config.clone();
        config.pipeline.write_archive_footer = true;
        let id = world.add_file("a.bin", &[1u8; 8][..]);
        world.seed_staged_row(id);
        world.place_symlink(id);
        let sid = world.begin_session(0);
        world.db.mark_tar_writer_session_pending(id, sid).expect("mark appended");

        let writer = TarWriter::open(
            config.paths.archive_path.clone(),
            &config.compression,
            config.process.jobs,
            world.shutdown.clone(),
            true,
        ).expect("open writer");
        let rt = ArchiveRTArgs {
            config: &config,
            db: &world.db,
            shutdown: &world.shutdown,
            progress: &world.progress,
        };
        let mut recorder = Recorder::new(&world.db, true, None);
        end_session(writer, &rt, sid, 0, true, &mut recorder).expect("end session");

        assert!(archive_footer::has_valid_footer(&config.paths.archive_path));
        assert_eq!(world.session_finalized(sid), 1);
    }

    // -------------------------------------------------------------------------
    // run() level
    // -------------------------------------------------------------------------

    #[test]
    fn run_finishes_archive() {
        let world = TestWorld::new();
        let ids: Vec<FileId> = (0..3)
            .map(|i| world.add_file(&format!("f{i}.bin"), &[u8::try_from(i).unwrap(); 16][..]))
            .collect();
        for id in &ids { world.seed_staged_row(*id); }
        for id in &ids { world.place_symlink(*id); }

        run(&world.rt()).expect("run ok");

        for id in &ids {
            assert_eq!(world.phase(*id), FilePhase::Archived);
            let flags = world.flags(*id);
            assert!(world.session_of(*id).is_some(), "payload recorded in a session");
            assert!(!flags.get(FileFlag::ErrorWhileArchive));
        }
        // Session finalized with byte counters recorded.
        assert!(world.db.has_finalized_archive_session().expect("has finalized"));
        let len = arrow_archive_len(&world);
        let out = world.db.get_archive_bytes_out().expect("bytes out").expect("recorded");
        // No footer in this config -> archive len equals recorded bytes out.
        assert_eq!(len, out);
    }

    #[test]
    fn run_resume_completes_missing_footer_when_enabled() {
        // A footerless-but-finalized archive (stream closed + session finalized,
        // interrupted before the trailer) is finished on resume by a footer-only
        // step — the resumable-fallback path for a mid-footer-hash interrupt.
        let world = TestWorld::new();
        let mut cfg = world.config.clone();
        cfg.pipeline.write_archive_footer = false; // pass 1 writes no trailer
        let id = world.add_file("a.bin", &[1u8; 8][..]);
        world.seed_staged_row(id);
        world.place_symlink(id);
        run(&ArchiveRTArgs { config: &cfg, db: &world.db, shutdown: &world.shutdown,
                             progress: &world.progress })
            .expect("first run");
        assert!(!archive_footer::has_valid_footer(&cfg.paths.archive_path));
        assert!(world.db.has_finalized_archive_session().expect("has finalized"));
        let bytes_out = world.db.get_archive_bytes_out().expect("bytes out").expect("recorded");

        // Resume with the trailer now enabled: append only the footer, advance to Done.
        cfg.pipeline.write_archive_footer = true;
        let fresh = Shutdown::detached();
        run(&ArchiveRTArgs { config: &cfg, db: &world.db, shutdown: &fresh,
                             progress: &world.progress })
            .expect("resume appends footer");
        assert!(archive_footer::has_valid_footer(&cfg.paths.archive_path));
        // bytes_out still records the tar-content length (footer is out-of-band);
        // the archive file has grown past it.
        assert_eq!(world.db.get_archive_bytes_out().expect("bytes out").expect("recorded"),
                   bytes_out);
        assert!(arrow_archive_len(&world) > bytes_out,
                "footer must extend the archive beyond the recorded tar bytes");
    }

    /// Fresh world with `archived` + `staged` counts and a session that is either
    /// finalized or left open; returns the `footer_pending` verdict.
    fn footer_state(archived: usize, staged: usize, finalize: bool, footer_enabled: bool) -> bool {
        let world = TestWorld::new();
        let mut config = world.config.clone();
        config.pipeline.write_archive_footer = footer_enabled;
        let ids: Vec<FileId> = (0..(archived + staged))
            .map(|i| world.add_file(&format!("f{i}.bin"), &[i as u8; 4][..]))
            .collect();
        for id in &ids[..archived] { world.seed_archived_row(*id); }
        for id in &ids[archived..] { world.seed_staged_row(*id); }
        let sid = world.begin_session(0);
        if finalize { world.db.finalize_tar_writer_session(sid).expect("finalize session"); }
        let rt = ArchiveRTArgs {
            config: &config, db: &world.db, shutdown: &world.shutdown, progress: &world.progress,
        };
        footer_pending(&rt).expect("footer_pending")
    }

    #[test]
    fn footer_pending_distinguishes_footer_ready_from_interrupted_modes() {
        // Target: stream closed + session finalized + every row archived -> the
        // interrupted-footer-hash state and the only true fork.
        assert!(footer_state(2, 0, true, true));
        // Graceful mid-archive: session finalized but a row still staged
        // (not yet appended) -> must NOT footer an incomplete stream.
        assert!(!footer_state(1, 1, true, true));
        // Crash before session-finalize: all rows archived, session still open.
        assert!(!footer_state(2, 0, false, true));
        // Footer not requested.
        assert!(!footer_state(2, 0, true, false));
    }

    #[test]
    fn run_graceful_interrupt_then_resume() {
        let world = TestWorld::new();
        let ids: Vec<FileId> = (0..3)
            .map(|i| world.add_file(&format!("g{i}.bin"), &[u8::try_from(i).unwrap(); 16][..]))
            .collect();
        for id in &ids { world.seed_staged_row(*id); }
        for id in &ids { world.place_symlink(*id); }

        world.shutdown.request_graceful();
        let res = run(&world.rt());
        assert!(matches!(res, Err(Error::Interrupted)));

        // Session finalized gracefully (no tar EOF); nothing appended yet.
        assert!(world.db.has_finalized_archive_session().expect("has finalized"));
        // Resume with a fresh shutdown; completes.
        let fresh = Shutdown::detached();
        run(&world.rt_with(&fresh)).expect("resume ok");
        for id in &ids { assert_eq!(world.phase(*id), FilePhase::Archived); }
    }

    #[test]
    fn run_force_interrupt_then_resume() {
        let world = TestWorld::new();
        let ids: Vec<FileId> = (0..3)
            .map(|i| world.add_file(&format!("x{i}.bin"), &[u8::try_from(i).unwrap(); 16][..]))
            .collect();
        for id in &ids { world.seed_staged_row(*id); }
        for id in &ids { world.place_symlink(*id); }

        // Prior session already archived bytes -> run() skips the init-manifest
        // write (which would otherwise hang on `check_in_flight` under force).
        let prior = world.add_file("prior.bin", &[9u8; 32][..]);
        world.seed_archived_row(prior);

        world.shutdown.request_force();
        let res = run(&world.rt());
        assert!(matches!(res, Err(Error::Interrupted)));
        // No session finalized; nothing promoted; `prior` stays archived.
        assert!(!world.db.has_finalized_archive_session().expect("has finalized"));

        // Resume: recovery truncates the abandoned stream, then completes.
        let fresh = Shutdown::detached();
        run(&world.rt_with(&fresh)).expect("resume ok");
        for id in &ids { assert_eq!(world.phase(*id), FilePhase::Archived); }
    }

    #[test]
    fn run_canonicalize_failure_marks_and_continues() {
        let world = TestWorld::new();
        let bad = world.add_file("bad.bin", &[1u8; 8][..]);
        let good = world.add_file("good.bin", &[2u8; 8][..]);
        world.seed_staged_row(bad);
        world.seed_staged_row(good);
        // Break only `bad`'s staged symlink; `good` stays intact.
        world.place_symlink(good);
        let bad_link = world.place_symlink(bad);
        fs::remove_file(&bad_link).expect("remove symlink");

        run(&world.rt()).expect("run completes with soft canonicalize failure");

        let flags = world.flags(bad);
        assert!(flags.get(FileFlag::ErrorWhileArchive));
        assert_eq!(world.phase(bad), FilePhase::Archived);
        assert_eq!(world.phase(good), FilePhase::Archived);
    }

    #[test]
    fn run_canonicalize_failure_fail_fast_raises() {
        let mut world = TestWorld::new();
        world.config.process.fail_fast = true;
        let bad = world.add_file("bad.bin", &[1u8; 8][..]);
        world.seed_staged_row(bad);
        // Symlink never created -> canonicalize fails -> fail_fast raises.
        let res = run(&world.rt());
        assert!(matches!(res, Err(Error::FileStat(_))));
    }

    #[test]
    fn run_append_success_records_session() {
        let world = TestWorld::new();
        let id = world.add_file("a.bin", &[1u8; 8][..]);
        world.seed_staged_row(id);
        world.place_symlink(id);
        run(&world.rt()).expect("run ok");
        assert!(world.session_of(id).is_some(), "successful append records a session");
        assert_eq!(world.phase(id), FilePhase::Archived);
    }

    #[test]
    fn run_append_success_sets_modified_on_stale_times() {
        let world = TestWorld::new();
        let stale = DateTime::<Utc>::from_timestamp_millis(1).expect("stale ts");
        let id = world.add_file_recorded(
            "m.bin", &[1u8; 8][..],
            Some(stale), Some(stale), Some(stale),
        );
        world.seed_staged_row(id);
        world.place_symlink(id);
        run(&world.rt()).expect("run ok");
        assert!(world.flags(id).get(FileFlag::MtimeChanged));
    }

    /// chmod-000 append: read of the payload fails mid-tar-write; the member is
    /// marked ErrorWhileArchive and the run continues. Skipped as root (permissions
    /// are bypassed).
    #[test]
    fn run_append_permission_error_marks_and_continues() {
        if geteuid().is_root() {
            return;
        }
        let world = TestWorld::new();
        let unreadable = world.add_file("no.tx", &[1u8; 64][..]);
        let ok = world.add_file("ok.bin", &[2u8; 64][..]);
        world.seed_staged_row(unreadable);
        world.seed_staged_row(ok);
        world.place_symlink(unreadable);
        world.place_symlink(ok);
        let payload = world.record(unreadable).abs_path;
        let mut perms = fs::metadata(&payload).expect("meta").permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&payload, perms).expect("chmod 000");

        run(&world.rt()).expect("run continues past permission error");

        let flags = world.flags(unreadable);
        assert!(flags.get(FileFlag::ErrorWhileArchive));
        assert!(world.session_of(unreadable).is_none(), "failed append must not record a session");
        assert_eq!(world.phase(unreadable), FilePhase::Archived);
        assert_eq!(world.phase(ok), FilePhase::Archived);
    }

    fn arrow_archive_len(world: &TestWorld) -> u64 {
        fs::metadata(&world.config.paths.archive_path).expect("meta").len()
    }
}