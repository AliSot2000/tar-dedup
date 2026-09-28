use crate::archive::ArchiveRTArgs;
use crate::common::batched_stepped_loop;
use crate::common::files::warn_if_times_changed;
use crate::config::PipelinePhase;
use crate::db::flags::{ErrorFlags, FileFlag};
use crate::db::types::{FileId, FilePhase, StrippedRecord};
use crate::db::{ErrorPhase, Recorder};
use crate::error::{Error, Result};
use path_clean::PathClean;
use rayon::ThreadPoolBuilder;
use rayon::prelude::*;
use std::fs;
use std::mem::take;
use std::os::unix::fs::symlink;
use std::sync::Mutex;

const BATCH_SIZE: u64 = 10_000;

const ERROR_PHASE: ErrorPhase = ErrorPhase::Pipeline(PipelinePhase::Stage);

const EXPECTED_CANONICAL: &str = "stage: Expected only canonical files. \
                            Got wrong file type or non-canonical file";

pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    fs::create_dir_all(rt.config.paths.stage_dir())
        .map_err(|e| Error::io(&rt.config.paths.stage_dir(), e))?;

    let promoted = rt.db.promote_unstageable_files()?;
    rt.progress.inc_global(promoted);
    tracing::info!(promoted, "promoted entries to staged which aren't eligible");

    // Progress across sessions: the workload counts every eligible canonical file
    // (both sparsified todo and already-staged done); the position is the
    // difference, so a resumed run keeps showing prior progress.
    let workload = rt.db.count_all_stage_candidates()?;
    let pending = rt.db.count_files_in_phase(FilePhase::Sparsified)?;
    rt.progress.set_phase_total(workload);
    rt.progress.set_phase_position(workload.saturating_sub(pending));
    if pending == 0 {
        tracing::info!("stage complete (nothing to do)");
        return Ok(());
    }

    let mut recorder = Recorder::new(rt.db, !rt.config.process.no_errors);
    let pool = ThreadPoolBuilder::new()
        .num_threads(rt.config.process.io_jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;
    let shutdown = rt.shutdown.clone();
    let mut staged: u64 = 0;

    batched_stepped_loop(
        BATCH_SIZE,
        || FileId(0),
        |lid: &FileId, batch_size| rt.db.list_files_to_stage_after(lid, batch_size),
        |r: &StrippedRecord| r.id,
        |entries: Vec<StrippedRecord> | {
            let results: Mutex<Vec<std::result::Result<(FileId, bool), (FileId, bool, Error)>>> =
                Mutex::new(Vec::new());

            let parallel = pool.install(|| {
                entries.par_iter().try_for_each(|record| -> Result<()> {
                    shutdown.check_between_files()?;

                    let source = if record.flags.get(FileFlag::HasSparse) {
                        let sparse_name = record.sparse_member_name().expect(EXPECTED_CANONICAL);
                        rt.config.paths.stage_dir().join(sparse_name).clean()
                    } else {
                        record.abs_path.to_path_buf()
                    };

                    let tar_name = record.tar_member_name().expect(EXPECTED_CANONICAL);
                    let modified = warn_if_times_changed(
                        &source,
                        record.mtime,
                        record.atime,
                        record.ctime,
                    );
                    debug_assert_eq!(source, source.clean(), "Source Paths must be normalized");
                    let target = rt.config.paths.stage_dir().join(tar_name);
                    if target.exists() {
                        match fs::remove_file(&target) {
                            Ok(()) => (),
                            Err(e) => {
                                results.lock().expect("stage results lock").push(
                                    Err((record.id, modified, Error::copy_io(&target, &e))));
                                return Err(Error::io(&target, e));
                            }
                        }
                    }
                    match symlink(&source, &target) {
                        Ok(()) => {
                            results.lock().expect("stage results lock").push(
                                Ok((record.id, modified))
                            );
                            Ok(())
                        }
                        Err(e) => {
                            results.lock().expect("stage results lock").push(
                                Err((record.id, modified, Error::copy_io(&target, &e))));
                            // Short-circuit the batch: bounds wasted syscalls on a
                            // structural failure (perm / disk full / read-only FS).
                            Err(Error::io(&target, e))
                        }
                    }
                })
            });

            // Main-thread apply only; workers never touch the Database.
            for outcome in take(&mut *results.lock().expect("stage results lock")) {
                match outcome {
                    Ok((id, modified)) => {
                        rt.db.mark_file_phase(id, FilePhase::Staged)?;
                        if modified { rt.db.set_file_flag(id, FileFlag::Modified, true)?; }
                        staged += 1;
                        rt.progress.inc_both(1);
                    }
                    Err((id, modified, err)) => {
                        if modified { rt.db.set_file_flag(id, FileFlag::Modified, true)?; }
                        match err {
                            Error::Interrupted => (),
                            Error::FileStat(fse) => {
                                let cp_err = fse.recreate();
                                recorder.record_file(
                                    id, ERROR_PHASE, fse, ErrorFlags::default(),
                                );
                                return Err(Error::FileStat(cp_err));
                            }
                            other => panic!(
                                "INVARIANT FAILED: stage worker may only return \
                                FileStat/Interrupted. Got: {other} on file {id:?}"
                            ),
                        }
                    }
                }
            }

            match parallel {
                Ok(()) => (),
                Err(Error::Interrupted) => {
                    recorder.flush()?;
                    return Err(Error::Interrupted);   // resumable
                }
                Err(e) => {
                    recorder.flush()?;
                    return Err(e);   // fail-fast to caller
                }
            }
            Ok(())
        }
    )?;

    recorder.flush()?;
    tracing::info!(staged, "stage complete");
    // Live DB already lives in the flat work dir (`snapshot.sqlite`); tar-writer
    // stages a copy via `.snapshot-for-tar.sqlite` when appending to the archive.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::files::original_extension;
    use crate::common::start::StartPolicy;
    use crate::config::{
        ArchiveConfig, ArchivePipelineOptions, CaptureOptions, CleanupSettings, CompressionFormat,
        CompressionSettings, FilterOptions, IndexingOptions, InputOptions, OwnerPolicy, PathLayout,
        ProcessOptions, SparseOptions,
    };
    use crate::db::Database;
    use crate::db::flags::FileFlag;
    use crate::db::types::{FileId, FileType, NewFileRecord};
    use crate::progress::{ARCHIVE_MULTIPLIER, ProgressBarSet};
    use chrono::{DateTime, Utc};
    use nix::unistd::geteuid;
    use rusqlite::named_params;
    use std::os::unix::fs::PermissionsExt;
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
            sparse: SparseOptions { sparsify: true, page_size: 4096, min_pages: 4 },
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
            let db = Database::open(&dir.path().join("test.sqlite")).expect("open db");
            // The internal include rule the filter phase installs: without it
            // `apply_no_filter` (-1) violates the FK.
            db.with_transaction(|conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO filter_reason_archive (id, source, line, expression) \
                     VALUES (-1, 'internal', NULL, '.*')", [],
                ).expect("seed internal include rule");
                Ok(())
            }).expect("seed internal include rule tx");
            let mut config = test_archive_config();
            config.paths.work_dir = dir.path().join("astage");
            // `run()` creates the stage dir itself (== work dir), so make it up
            // front to also mirror a pre-existing resume layout.
            fs::create_dir_all(&config.paths.work_dir).expect("create stage dir");
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

        fn add_file(&self, name: &str, payload: &[u8]) -> FileId {
            let path = self.dir.path().join(name);
            std::fs::write(&path, payload).expect("write test payload");
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
            std::fs::write(&path, payload).expect("write test payload");
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

        /// The canonical row for `id` as `sparsified`: the exact precondition the
        /// stage list evaluates. Sets the filter columns so the row is eligible.
        fn seed_stage_row(&self, id: FileId) {
            self.db.with_transaction(|conn| {
                let n = conn.execute(
                    "UPDATE files SET phase = 'sparsified', canonical_id = :id, \
                     sha1 = :sha1, include_reason_archive = -1, exclude_reason_archive = 0 \
                     WHERE id = :id",
                    named_params! {
                        ":id": id.0,
                        ":sha1": [7u8; 20].as_slice(),
                    },
                ).expect("seed stage row");
                assert_eq!(n, 1);
                Ok(())
            }).expect("seed stage row tx");
        }

        /// Mark `id` as already staged (prior session) without touching its row
        /// further — the stage phase must leave it alone.
        fn seed_staged_row(&self, id: FileId) {
            self.db.with_transaction(|conn| {
                let n = conn.execute(
                    "UPDATE files SET phase = 'staged' WHERE id = :id",
                    named_params! { ":id": id.0 },
                ).expect("seed staged row");
                assert_eq!(n, 1);
                Ok(())
            }).expect("seed staged row tx");
        }

        fn phase(&self, id: FileId) -> FilePhase {
            self.db.get_file_by_id::<StrippedRecord>(id)
                .expect("get row")
                .expect("row present")
                .phase
        }

        fn flag(&self, id: FileId, flag: FileFlag) -> bool {
            self.db.get_file_flag(id, flag).expect("get flag")
        }

        fn record(&self, id: FileId) -> StrippedRecord {
            self.db.get_file_by_id::<StrippedRecord>(id)
                .expect("get row")
                .expect("row present")
        }

        fn errors_for(&self, id: FileId) -> Vec<crate::db::ErrorRecord> {
            self.db.get_records_by_file_id(id).expect("get records")
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
    }

    #[test]
    fn run_aborts_when_stage_dir_uncreatable() {
        let world = TestWorld::new();
        let id = world.add_file(format!("a.bin").as_str(), &[1u8; 8][..]);
        world.seed_stage_row(id);
        // Replace the (auto-created) stage dir with a regular file so
        // `create_dir_all` in run() fails before anything else.
        std::fs::remove_dir_all(&world.config.paths.work_dir).expect("remove stage dir");
        std::fs::write(&world.config.paths.work_dir, &[0u8; 1][..]).expect("write blocker");

        let res = run(&world.rt());
        assert!(matches!(res, Err(Error::FileStat(_))));
        if let Err(Error::FileStat(fse)) = res {
            assert_eq!(fse.io_path(), Some(world.config.paths.work_dir.clone()));
        }
        // Nothing was staged and no phase advanced.
        assert_eq!(world.phase(id), FilePhase::Sparsified);
        assert!(world.errors_for(id).is_empty());
    }

    #[test]
    fn run_with_no_pending_returns_ok() {
        let world = TestWorld::new();
        let id = world.add_file(format!("a.bin").as_str(), &[1u8; 8][..]);
        world.seed_stage_row(id);
        world.seed_staged_row(id);   // nothing left sparsified

        run(&world.rt()).expect("run ok");

        assert_eq!(world.phase(id), FilePhase::Staged);
        assert!(world.errors_for(id).is_empty());
    }

    #[test]
    fn run_retains_progress_and_does_not_redo_work() {
        let world = TestWorld::new();
        let mut pre_staged = Vec::<FileId>::new();
        for i in [1, 2] {
            let id = world.add_file(format!("pre{i}.bin").as_str(), &[i as u8; 64][..]);
            world.seed_stage_row(id);
            world.seed_staged_row(id);
            world.place_symlink(id);
            pre_staged.push(id);
        }
        let mut sparsified = Vec::<FileId>::new();
        for n in [1, 2, 3, 4] {
            let id = world.add_file(format!("f{n}.bin").as_str(), &[n as u8; 64][..]);
            world.seed_stage_row(id);
            sparsified.push(id);
        }
        // Capture each pre-staged symlink's target before the run.
        let mut pre_targets = Vec::<PathBuf>::new();
        for id in pre_staged.iter() {
            let target = world.target_path(*id);
            pre_targets.push(fs::read_link(&target).expect("read pre-staged link"));
        }

        // Session 1 stages only the 4 pending rows; the 2 pre-staged rows are
        // untouched (progress retention).
        run(&world.rt()).expect("run 1");

        for id in sparsified {
            assert_eq!(world.phase(id), FilePhase::Staged);
            assert!(world.target_path(id).is_symlink());
            assert!(world.errors_for(id).is_empty());
        }
        for (i, id) in pre_staged.iter().enumerate() {
            assert_eq!(world.phase(*id), FilePhase::Staged);
            // The pre-staged links were not recreated: identical targets.
            let target = world.target_path(*id);
            assert_eq!(fs::read_link(&target).expect("read link"), pre_targets[i]);
        }
        assert_eq!(world.db.count_files_in_phase(FilePhase::Sparsified).expect("count"), 0);
        assert_eq!(world.db.count_files_in_phase(FilePhase::Staged).expect("count"), 6);

        // Session 2 is a no-op: nothing left to do, nothing redone.
        run(&world.rt()).expect("run 2");
        assert_eq!(world.db.count_files_in_phase(FilePhase::Sparsified).expect("count"), 0);
        assert_eq!(world.db.count_files_in_phase(FilePhase::Staged).expect("count"), 6);
    }

    #[test]
    fn run_detects_and_stores_mtime_change() {
        let world = TestWorld::new();
        let stale = DateTime::from_timestamp(Utc::now().timestamp() - 3600, 0).expect("stale");
        let id = world.add_file_recorded("a.bin", &[7u8; 64], Some(stale), None, None);
        world.seed_stage_row(id);

        run(&world.rt()).expect("run ok");

        assert_eq!(world.phase(id), FilePhase::Staged);
        assert!(world.flag(id, FileFlag::Modified));
        assert!(world.target_path(id).is_symlink());
    }

    #[test]
    fn run_removes_stale_target_and_stages() {
        let world = TestWorld::new();
        let id = world.add_file(format!("a.bin").as_str(), &[1u8; 8][..]);
        world.seed_stage_row(id);
        // A stale non-symlink at the target (e.g. leftover from an interrupted run).
        let target = world.target_path(id);
        std::fs::write(&target, &[2u8; 3][..]).expect("write stale target");

        run(&world.rt()).expect("run ok");

        assert!(target.is_symlink());
        let rec = world.record(id);
        assert_eq!(fs::read_link(&target).expect("read link"), rec.abs_path);
        assert_eq!(world.phase(id), FilePhase::Staged);
        assert!(world.errors_for(id).is_empty());
    }

    #[test]
    fn run_target_removal_failure_aborts_and_prevents_further_staging() {
        let world = TestWorld::new();
        let mut ids = Vec::<FileId>::new();
        for (n, byte) in [(1, 10), (2, 20), (3, 30)] {
            let id = world.add_file(format!("f{n}.bin").as_str(), &[byte; 64][..]);
            world.seed_stage_row(id);
            ids.push(id);
        }
        // Unremovable stale target on the middle file: an empty directory cannot
        // be `unlink`ed.
        let middle = ids[1];
        let dir = world.target_path(middle);
        fs::create_dir_all(&dir).expect("create blocking dir");

        let res = run(&world.rt());
        assert!(matches!(res, Err(Error::FileStat(_))));

        // The failing middle file is recorded (Stage error) and stays sparsified.
        let records = world.errors_for(middle);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].file_id, Some(middle));
        assert_eq!(records[0].phase, ErrorPhase::Pipeline(PipelinePhase::Stage));
        assert_eq!(world.phase(middle), FilePhase::Sparsified);
        // Strict tail assertion: ids after the failing one were never staged.
        assert_eq!(world.phase(ids[0]), FilePhase::Staged);
        assert_eq!(world.phase(ids[2]), FilePhase::Sparsified);
    }

    #[test]
    fn run_symlink_failure_recovers_without_reset() {
        if geteuid().is_root() {
            return;   // chmod 0555 does not block root
        }
        let world = TestWorld::new();
        let id = world.add_file(format!("a.bin").as_str(), &[1u8; 8][..]);
        world.seed_stage_row(id);

        // Read-only stage dir: `symlink` fails, no flag/phase mutation occurs.
        std::fs::set_permissions(
            &world.config.paths.work_dir,
            std::fs::Permissions::from_mode(0o555),
        ).expect("chmod 0555");
        let res = run(&world.rt());
        std::fs::set_permissions(
            &world.config.paths.work_dir,
            std::fs::Permissions::from_mode(0o755),
        ).expect("chmod back 0755");

        assert!(matches!(res, Err(Error::FileStat(_))));
        assert_eq!(world.phase(id), FilePhase::Sparsified);
        assert!(!world.flag(id, FileFlag::Modified));
        let records = world.errors_for(id);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].phase, ErrorPhase::Pipeline(PipelinePhase::Stage));
        // No symlink was created by the failed attempt.
        assert_ne!(world.target_path(id).is_symlink(), true);

        // Fix the problem, run again: no reset needed, the row is re-staged.
        run(&world.rt()).expect("rerun ok");
        assert_eq!(world.phase(id), FilePhase::Staged);
        assert!(world.target_path(id).is_symlink());
    }

    #[test]
    fn run_stages_a_dozen_and_updates_db() {
        let world = TestWorld::new();
        let mut ids = Vec::<FileId>::new();
        for n in 0..12 {
            let id = world.add_file(format!("f{n}.bin").as_str(), &[n as u8; 64][..]);
            world.seed_stage_row(id);
            ids.push(id);
        }

        run(&world.rt()).expect("run ok");

        for id in ids {
            assert_eq!(world.phase(id), FilePhase::Staged);
            let target = world.target_path(id);
            assert!(target.is_symlink());
            let rec = world.record(id);
            assert_eq!(fs::read_link(&target).expect("read link"), rec.abs_path);
            assert!(world.errors_for(id).is_empty());
        }
        assert_eq!(world.db.count_files_in_phase(FilePhase::Sparsified).expect("count"), 0);
        assert_eq!(world.db.count_files_in_phase(FilePhase::Staged).expect("count"), 12);
    }
}