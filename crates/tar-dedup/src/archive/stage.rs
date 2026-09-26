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
            let results: Mutex<Vec<std::result::Result<FileId, (FileId, Error)>>> =
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
                    warn_if_times_changed(
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
                                    Err((record.id, Error::copy_io(&target, &e))));
                                return Err(Error::io(&target, e));
                            }
                        }
                    }
                    match symlink(&source, &target) {
                        Ok(()) => {
                            results.lock().expect("stage results lock").push(Ok(record.id));
                            Ok(())
                        }
                        Err(e) => {
                            results.lock().expect("stage results lock").push(
                                Err((record.id, Error::copy_io(&target, &e))));
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
                    Ok(id) => {
                        rt.db.mark_file_phase(id, FilePhase::Staged)?;
                        staged += 1;
                        rt.progress.inc_both(1);
                    }
                    Err((_, Error::Interrupted)) => (),
                    Err((id, Error::FileStat(fse))) => {
                        recorder.record_file(id, ERROR_PHASE, fse, ErrorFlags::default());
                    }
                    Err((id, other)) => panic!(
                        "INVARIANT FAILED: stage worker may only return FileStat/Interrupted. \
                    Got: {other} on file {id:?}"
                    ),
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