use path_clean::PathClean;
use rayon::ThreadPoolBuilder;
use rayon::prelude::*;
use sparse_cp::sparse_copy_with_progress;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::archive::ArchiveRTArgs;
use crate::common::files::{PreYield, warn_if_times_changed};
use crate::config::ArchiveConfig;
use crate::db::Database;
use crate::db::ErrorPhase;
use crate::db::flags::ErrorFlags;
use crate::db::types::{FileId, FilePhase, StrippedRecord};
use crate::error::{Error, Result};
use crate::progress::ProgressBarSet;
use crate::shutdown::Shutdown;

enum SparseOutcome {
    Ok(FileId),
    Err(FileId, Error),
}

const ERROR_PHASE: ErrorPhase = ErrorPhase::Pipeline(crate::config::PipelinePhase::Sparsify);

/// Deletes `path` on drop unless [`keep`](Self::keep) was called.
struct TempSparseFile {
    path: PathBuf,
    keep: bool,
}

impl TempSparseFile {
    fn new(path: PathBuf) -> Self {
        Self { path, keep: false }
    }

    /// Mark path to be kept.
    fn keep(mut self) {
        self.keep = true;
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempSparseFile {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Sparsify stage: optional sparse rewrites under `stage/sp.{content_id}`.
pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    let config = rt.config;
    let db = rt.db;
    let shutdown = rt.shutdown;
    debug_assert_ne!(config.sparse.page_size, 0, "Expected page_size > 0");
    if config.sparse.page_size == 0 {
        return Err(Error::Config("page_size must be greater than 0".into()));
    }

    tracing::info!(
        page_size = config.sparse.page_size,
        min_pages = ?config.sparse.min_pages,
        "sparsify pass"
    );

    let stage_dir = config.paths.stage_dir();
    fs::create_dir_all(&stage_dir).map_err(|e| Error::io(&stage_dir, e))?;

    if !config.sparse.sparsify {
        let n = db.promote_deduped_to_sparsified()?;
        rt.progress.inc_global(n);
        tracing::info!(count = n, "promoted all deduped → sparsified (min_pages unset)");
        return Ok(());
    };

    // PRECONDITION: min_page set.
    let skipped = db.promote_non_sparsify_candidates_to_sparsified(config.sparse.min_pages)?;
    rt.progress.inc_global(skipped);
    tracing::info!(count = skipped, "promoted non-candidates → sparsified");

    // TODO batching!
    let candidates: Vec<StrippedRecord> = db.list_sparsify_candidates(config.sparse.min_pages)?;
    if candidates.is_empty() {
        sanity_no_deduped(db)?;
        return Ok(());
    }

    let progress = rt.progress;
    progress.set_phase_total(candidates.len() as u64);
    let results = Mutex::new(Vec::<SparseOutcome>::with_capacity(candidates.len()));

    let checked = PreYield::new(candidates.into_iter(), |record: &StrippedRecord| {
        warn_if_times_changed(&record.abs_path, record.mtime, record.atime, record.ctime);
    });

    let parallel = run_pool(config, shutdown, progress, &results, checked);

    let outcomes = results.into_inner().expect("sparsify results lock");
    let saved = outcomes.len();
    let mut ok = 0u64;
    let mut err = 0u64;
    let mut recorder = crate::db::Recorder::new(db, !config.process.no_errors);
    for outcome in outcomes {
        match outcome {
            SparseOutcome::Ok(id) => { db.mark_sparsified_sparse(id)?; ok += 1; }
            SparseOutcome::Err(_id, Error::Interrupted) => (),
            SparseOutcome::Err(id, Error::FileStat(e)) => {
                db.mark_sparsified_error(id)?;
                recorder.record_file(id, ERROR_PHASE, e, ErrorFlags::default());
                err += 1;
            }
            SparseOutcome::Err(_id, e) => panic!(
                "PRECONDITION FAILED: Expected Error::Interrupted or Error::FileStatError, \
                found: {e}"
            ),
        }
    }
    recorder.flush()?;

    match parallel {
        Ok(()) => {
            sanity_no_deduped(db)?;
            tracing::info!(ok, err, "sparsify complete");
            Ok(())
        }
        Err(Error::Interrupted) => {
            tracing::warn!(saved, "sparsify interrupted; completed files saved");
            Err(Error::Interrupted)
        }
        Err(e) => Err(e),
    }
}

/// Run the rayon pool over sparsify candidates and return its result.
fn run_pool(
    config: &ArchiveConfig,
    shutdown: &Shutdown,
    progress: &ProgressBarSet,
    results: &Mutex<Vec<SparseOutcome>>,
    checked: impl Iterator<Item = StrippedRecord> + Send,
) -> Result<()> {
    let pool = ThreadPoolBuilder::new()
        .num_threads(config.process.io_jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    let stage_dir = config.paths.stage_dir().clone();
    let page_size = config.sparse.page_size;
    let shutdown_workers = shutdown.clone();

    let parallel = pool.install(|| {
        checked.par_bridge().try_for_each(|record| {
            shutdown_workers.check_between_files()?;

            let name = record
                .sparse_member_name()
                .expect("Invariant: sparsify candidates must be self-canonical files");
            let dst = stage_dir.join(name).clean();
            let tmp = TempSparseFile::new(dst);

            let copy_result =
                sparse_copy_with_progress(&record.abs_path, tmp.path(), page_size, |_, _, _| {
                    if shutdown_workers.is_force() {
                        Err(Error::Interrupted)
                    } else {
                        Ok(())
                    }
                });

            match copy_result {
                Ok(_) => {
                    tmp.keep();
                    results
                        .lock()
                        .expect("sparsify results lock poisoned")
                        .push(SparseOutcome::Ok(record.id));
                    progress.inc_both(1);
                    Ok(())
                }
                Err(Error::Interrupted) => {
                    drop(tmp);
                    Err(Error::Interrupted)
                }
                Err(e @ Error::FileStat(_)) => {
                    drop(tmp);
                    results
                        .lock()
                        .expect("sparsify results lock poisoned")
                        .push(SparseOutcome::Err(
                            record.id,
                            e,
                        ));
                    progress.inc_both(1);
                    Ok(())
                }
                Err(other) => panic!(
                    "Contract violated, only FileStatErrors and Interrupted expected. Got: {other}"
                ),
            }
        })
    });
    parallel
}

/// Sanity check that we have files left in the dedup phase.
fn sanity_no_deduped(db: &Database) -> Result<()> {
    let leftover = db.count_files_in_phase(FilePhase::Deduped)?;
    if leftover != 0 {
        panic!(
            "INVARIANT ERROR: \
             sparsify finished with {leftover} file(s) still in deduped (expected 0)"
        );
    }
    Ok(())
}
