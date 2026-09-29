use std::fs::OpenOptions;
use std::path::Path;
use std::{fs, io};

use crate::archive::ArchiveRTArgs;
use crate::archive_footer;
use crate::common::batched_stepped_loop;
use crate::common::files::warn_if_times_changed;
use crate::common::{SNAPSHOT_INIT_TAR_NAME, SNAPSHOT_TAR_NAME};
use crate::db::ErrorPhase;
use crate::db::flags::{ErrorFlags, FileFlag};
use crate::db::types::{FilePhase, StrippedRecord};
use crate::db::{Database, Recorder};
use crate::error::{Error, FileStatError, Result};
use crate::tar_writer::TarWriter;

const ERROR_PHASE: ErrorPhase = ErrorPhase::Pipeline(crate::config::PipelinePhase::Archive);

const BATCH_SIZE: u64 = 10_000;

// INFO - Archival works as follows:
//  Ineligible files are promoted first.
//  Then, the eligible files are appended to the archive one by one.
//  If it fails, the ErrorWhileArchive is set
//  If it succeeds the AppendedPath is set
//  If a session is force aborted, the stream is abandoned and the database committed
//  If a session is exiting gracefully or was not interrupted, the set the phase of the files with
//  the ArchivedPath to 'archived'. Then we append a snapshot of the database into the stream prior
//  to finalizing and closing the stream.
//  (??? Files with ErrorWhileArchive need to be promoted to 'archived')

pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    let mut recorder = Recorder::new(rt.db, !rt.config.process.no_errors);
    // Crash / force leftover: truncate incomplete stream C, keep finished A.B.
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

    // Batched queue of staged canonicals in packing order; the ordering table
    // survives interrupts so a resume repopulate is a no-op for queued rows.
    rt.db.create_archive_queue()?;
    rt.db.populate_archive_queue(false)?; // sort_by_name hard-coded false for now
    let to_archive = rt.db.count_files_in_phase(FilePhase::Staged)?;
    if to_archive == 0 && already_archived == 0 {
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

    // Process the queue in batches. `pull_pending_archive_rows` advances the read
    // cursor by the last queue position of each batch; already-appended /
    // errored rows exit the pull until the next session.
    match batched_stepped_loop(
        BATCH_SIZE,
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
                        rt.db.promote_to_archived(&record.id)?;
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
                        capture_error(e, &mut recorder, record.id);
                        rt.db.set_file_flag(record.id, FileFlag::ErrorWhileArchive, true)?;
                        rt.db.promote_to_archived(&record.id)?;
                        // Do not set AppendedPath — member was not successfully written.
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
        },
    ) {
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
    use crate::common::files::original_extension;
    use crate::common::start::StartPolicy;
    use crate::config::{
        ArchiveConfig, ArchivePipelineOptions, CaptureOptions, CleanupSettings, CompressionFormat,
        CompressionSettings, FilterOptions, IndexingOptions, InputOptions, OwnerPolicy, PathLayout,
        ProcessOptions, SparseOptions,
    };
    use crate::db::Database;
    use crate::db::flags::{ErrorScope, FileFlag};
    use crate::db::types::{FileId, FileType, NewFileRecord};
    use crate::progress::{ARCHIVE_MULTIPLIER, ProgressBarSet};
    use crate::shutdown::Shutdown;
    use chrono::{DateTime, Utc};
    use nix::unistd::geteuid;
    use rusqlite::named_params;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::PathBuf;

    fn test_archive_config() -> ArchiveConfig {
        ArchiveConfig {
            paths: PathLayout {
                archive_path: PathBuf::new(),
                directory: PathBuf::new(),
                work_dir: PathBuf::new(),
            },
            inputs: InputOptions {
                input_dirs: Vec::new(),
                files_from: Vec::new(),
                files_from_null: false,
            },
            indexing: IndexingOptions {
                no_recursion: false,
                dereference: false,
                one_file_system: false,
                no_hardlink_detection: true,
                no_strict_separation: false,
            },
            filter: FilterOptions {
                exclude_patterns: Vec::new(),
                include_patterns: Vec::new(),
                exclude_from: Vec::new(),
                include_from: Vec::new(),
                anchored: false,
                ignore_case: false,
                eager_filter: false,
            },
            capture: CaptureOptions {
                do_xattrs: false,
                do_posix_acl: false,
                do_selinux: false,
                numeric_ids_only: false,
                mode: None,
                transform: None,
            },
            owner_policy: OwnerPolicy {
                owner: None,
                owner_map: None,
                group: None,
                group_map: None,
            },
            sparse: SparseOptions { sparsify: false, page_size: 4096, min_pages: 4 },
            compression: CompressionSettings {
                format: CompressionFormat::None,
                level: 0,
                xz_extreme: false,
                memlimit_compress: None,
            },
            process: ProcessOptions {
                start_policy: StartPolicy::Create,
                jobs: 4,
                io_jobs: 1,
                fail_fast: false,
                no_errors: false,
                cleanup: CleanupSettings { keep_db: false, keep_stage: false },
                exit_after_stage: None,
            },
            pipeline: ArchivePipelineOptions {
                dedup_mode: crate::cli::DedupMode::Regular,
                write_archive_footer: false,
                clear_archive_meta: false,
            },
        }
    }

    struct TestWorld {
        dir: tempfile::TempDir,
        db: Database,
        shutdown: Shutdown,
        progress: ProgressBarSet,
        config: ArchiveConfig,
    }

    impl TestWorld {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let mut config = test_archive_config();
            config.paths.work_dir = dir.path().join("astage");
            config.paths.archive_path = dir.path().join("test.astar");
            fs::create_dir_all(&config.paths.work_dir).expect("create stage dir");
            // append_snapshot mirrors db_path() into the tar; the DB must live there.
            let db = Database::open(&config.paths.db_path()).expect("open db");
            // The internal include rule the filter phase installs: without it
            // `apply_no_filter` (-1) violates the FK.
            db.with_transaction(|conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO filter_reason_archive (id, source, line, expression) \
                     VALUES (-1, 'internal', NULL, '.*')", [],
                ).expect("seed internal include rule");
                Ok(())
            }).expect("seed internal include rule tx");
            Self {
                dir, db,
                shutdown: Shutdown::detached(),
                progress: ProgressBarSet::new(ARCHIVE_MULTIPLIER),
                config,
            }
        }

        fn rt(&self) -> ArchiveRTArgs<'_> {
            ArchiveRTArgs {
                config: &self.config,
                db: &self.db,
                shutdown: &self.shutdown,
                progress: &self.progress,
            }
        }

        fn rt_with<'a>(&'a self, shutdown: &'a Shutdown) -> ArchiveRTArgs<'a> {
            ArchiveRTArgs {
                config: &self.config,
                db: &self.db,
                shutdown,
                progress: &self.progress,
            }
        }

        fn add_file(&self, name: &str, payload: &[u8]) -> FileId {
            let path = self.dir.path().join(name);
            fs::write(&path, payload).expect("write test payload");
            self.db.insert_file(&NewFileRecord {
                abs_path: PathBuf::from(&path),
                ext: original_extension(&path),
                size: payload.len() as u64,
                mtime: None, atime: None, ctime: None,
                uid: None, gid: None, mode: None,
                ftype: Some(FileType::File),
                xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None, link_dst: None,
                device_id: None, inode_id: None, major: None, minor: None,
            }).expect("insert test file");
            self.db.file_id_by_abs_path(&path).expect("lookup test file").expect("file present")
        }

        fn add_file_recorded(
            &self, name: &str, payload: &[u8],
            mtime: Option<DateTime<Utc>>, atime: Option<DateTime<Utc>>, ctime: Option<DateTime<Utc>>)
            -> FileId {
            let path = self.dir.path().join(name);
            fs::write(&path, payload).expect("write test payload");
            self.db.insert_file(&NewFileRecord {
                abs_path: PathBuf::from(&path),
                ext: original_extension(&path),
                size: payload.len() as u64,
                mtime, atime, ctime,
                uid: None, gid: None, mode: None,
                ftype: Some(FileType::File),
                xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None, link_dst: None,
                device_id: None, inode_id: None, major: None, minor: None,
            }).expect("insert recorded file");
            self.db.file_id_by_abs_path(&path).expect("lookup test file").expect("file present")
        }

        /// The canonical row for `id` as `staged`: the exact precondition the
        /// archive queue evaluates. Sets the filter columns so the row is eligible.
        fn seed_staged_row(&self, id: FileId) {
            self.db.with_transaction(|conn| {
                let n = conn.execute(
                    "UPDATE files SET phase = 'staged', canonical_id = :id, \
                     sha1 = :sha1, include_reason_archive = -1, exclude_reason_archive = 0 \
                     WHERE id = :id",
                    named_params! {
                        ":id": id.0,
                        ":sha1": [7u8; 20].as_slice(),
                    },
                ).expect("seed staged row");
                assert_eq!(n, 1);
                Ok(())
            }).expect("seed staged row tx");
        }

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

        fn phase(&self, id: FileId) -> FilePhase {
            self.db.get_file_by_id::<StrippedRecord>(id)
                .expect("get row")
                .expect("row present")
                .phase
        }

        fn flags(&self, id: FileId) -> crate::db::flags::FileFlags {
            self.db.get_file_flags(id).expect("get flags")
        }

        fn record(&self, id: FileId) -> StrippedRecord {
            self.db.get_file_by_id::<StrippedRecord>(id)
                .expect("get row")
                .expect("row present")
        }

        /// Absolute `{stage_dir}/{tar member}` target path for `id`.
        fn target_path(&self, id: FileId) -> PathBuf {
            let name = self.record(id).tar_member_name().expect("canonical");
            self.config.paths.stage_dir().join(name)
        }

        /// Create the symlink stage would create, and return the path.
        fn place_symlink(&self, id: FileId) -> PathBuf {
            let rec = self.record(id);
            let target = self.target_path(id);
            symlink(&rec.abs_path, &target).expect("place symlink");
            target
        }

        /// Open a session row (open = finalized 0) and return its id.
        fn begin_session(&self, offset: u64) -> i64 {
            self.db.begin_archive_session(offset).expect("begin session")
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
        world.db.finalize_archive_session(sid).expect("finalize");
        // bytes_out meta never recorded -> skip
        check_archive_bytes_out(&world.db, 999).expect("absent meta -> ok");
    }

    #[test]
    fn check_bytes_out_ok_when_equal() {
        let world = TestWorld::new();
        let sid = world.begin_session(0);
        world.db.finalize_archive_session(sid).expect("finalize");
        world.db.set_archive_bytes_out(42).expect("set bytes out");
        check_archive_bytes_out(&world.db, 42).expect("equal -> ok");
    }

    #[test]
    fn check_bytes_out_panics_on_mismatch() {
        let world = TestWorld::new();
        let sid = world.begin_session(0);
        world.db.finalize_archive_session(sid).expect("finalize");
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
        let mut recorder = Recorder::new(&world.db, true);
        recover_incomplete_session(&world.rt(), &mut recorder).expect("no session -> ok");
    }

    #[test]
    fn recover_incomplete_session_truncate_error_aborts() {
        let world = TestWorld::new();
        let mut recorder = Recorder::new(&world.db, true);
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

        let mut recorder = Recorder::new(&world.db, true);
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
        assert!(!world.flags(id).get(FileFlag::AppendedPath));
    }

    #[test]
    fn append_snapshot_uses_init_vs_regular_name() {
        let world = TestWorld::new();
        let mut recorder = Recorder::new(&world.db, true);

        let mut writer = TarWriter::open(
            world.config.paths.archive_path.clone(),
            &world.config.compression,
            world.config.process.jobs,
            world.shutdown.clone(),
            true,
        ).expect("open writer");
        append_snapshot(&mut writer, &world.rt(), true, &mut recorder).expect("init snapshot");
        writer.finalize_archive().expect("finalize init");

        let raw = fs::read(&world.config.paths.archive_path).expect("read archive");
        assert!(raw.windows(SNAPSHOT_INIT_TAR_NAME.len())
            .any(|w| w == SNAPSHOT_INIT_TAR_NAME.as_bytes()));
    }

    #[test]
    fn append_snapshot_staging_file_removed_after() {
        let world = TestWorld::new();
        let mut recorder = Recorder::new(&world.db, true);

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
        writer.finalize_archive().expect("finalize");
    }

    #[test]
    fn append_snapshot_graceful_does_not_abort() {
        let world = TestWorld::new();
        world.shutdown.request_graceful();
        let mut recorder = Recorder::new(&world.db, true);

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
        writer.finalize_archive().expect("finalize");
    }

    #[test]
    fn end_session_promotes_pending_to_archived() {
        let world = TestWorld::new();
        let id = world.add_file("a.bin", &[1u8; 8][..]);
        world.seed_staged_row(id);
        world.place_symlink(id);
        world.db.set_file_flag(id, FileFlag::AppendedPath, true).expect("mark appended");
        let sid = world.begin_session(0);

        let writer = TarWriter::open(
            world.config.paths.archive_path.clone(),
            &world.config.compression,
            world.config.process.jobs,
            world.shutdown.clone(),
            true,
        ).expect("open writer");
        let mut recorder = Recorder::new(&world.db, true);
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
        // No AppendedPath: promote_pending leaves it staged -> invariant panic.
        let sid = world.begin_session(0);

        let writer = TarWriter::open(
            world.config.paths.archive_path.clone(),
            &world.config.compression,
            world.config.process.jobs,
            world.shutdown.clone(),
            true,
        ).expect("open writer");
        let mut recorder = Recorder::new(&world.db, true);
        let res = catch_unwind(AssertUnwindSafe(|| {
            end_session(writer, &world.rt(), sid, 0, true, &mut recorder).expect("end session")
        }));
        assert!(res.is_err(), "full-archive close with staged rows must panic");
    }

    #[test]
    fn end_session_writes_footer_when_configured() {
        let world = TestWorld::new();
        let mut config = world.config.clone();
        config.pipeline.write_archive_footer = true;
        let id = world.add_file("a.bin", &[1u8; 8][..]);
        world.seed_staged_row(id);
        world.place_symlink(id);
        world.db.set_file_flag(id, FileFlag::AppendedPath, true).expect("mark appended");
        let sid = world.begin_session(0);

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
        let mut recorder = Recorder::new(&world.db, true);
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
            assert!(flags.get(FileFlag::AppendedPath));
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
    fn run_append_success_sets_appended_path() {
        let world = TestWorld::new();
        let id = world.add_file("a.bin", &[1u8; 8][..]);
        world.seed_staged_row(id);
        world.place_symlink(id);
        run(&world.rt()).expect("run ok");
        assert!(world.flags(id).get(FileFlag::AppendedPath));
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
        assert!(world.flags(id).get(FileFlag::Modified));
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
        assert!(!flags.get(FileFlag::AppendedPath));
        assert_eq!(world.phase(unreadable), FilePhase::Archived);
        assert_eq!(world.phase(ok), FilePhase::Archived);
    }

    fn arrow_archive_len(world: &TestWorld) -> u64 {
        fs::metadata(&world.config.paths.archive_path).expect("meta").len()
    }
}