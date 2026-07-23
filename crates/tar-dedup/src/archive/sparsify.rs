use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rayon::prelude::*;
use rayon::ThreadPoolBuilder;
use sparse_cp::sparse_copy_with_progress;

use crate::common::files::{warn_if_times_changed, PreYield};
use crate::config::Config;
use crate::db::types::{FileId, FilePhase, StrippedRecord};
use crate::db::Database;
use crate::error::{Error, Result};
use crate::progress::CountProgress;
use crate::shutdown::Shutdown;

enum SparseOutcome {
    Ok(FileId),
    Err(FileId),
}

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

fn warn_sparsify_times(input_dir: &Path, record: &StrippedRecord) {
    let path = input_dir.join(&record.rel_path);
    warn_if_times_changed(&path, record.mtime, record.atime, record.ctime);
}

/// Sparsify stage: optional sparse rewrites under `stage/sp.{content_id}`.
pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    // TODO check this in cli
    assert_ne!(config.page_size, 0, "Expected page_size > 0");
    if config.page_size == 0 {
        return Err(Error::Config("page_size must be greater than 0".into()));
    }

    tracing::info!(
        page_size = config.page_size,
        min_pages = ?config.min_pages,
        "sparsify pass"
    );

    let stage_dir = config.stage_dir();
    fs::create_dir_all(&stage_dir).map_err(|e| Error::io(&stage_dir, e))?;

    let Some(min_pages) = config.min_pages else {
        let n = db.promote_deduped_to_sparsified()?;
        tracing::info!(count = n, "promoted all deduped → sparsified (min_pages unset)");
        return Ok(());
    };

    // PRECONDITION: min_page set.
    let skipped = db.promote_non_sparsify_candidates_to_sparsified(min_pages)?;
    tracing::info!(count = skipped, "promoted non-candidates → sparsified");

    let candidates: Vec<StrippedRecord> = db.list_sparsify_candidates(min_pages)?;
    if candidates.is_empty() {
        sanity_no_deduped(db)?;
        return Ok(());
    }

    let bar = CountProgress::with_total("sparsify", candidates.len() as u64);
    let results = Mutex::new(Vec::<SparseOutcome>::with_capacity(candidates.len()));

    let input_dir = config.input_dir.clone();
    let checked = PreYield::new(candidates.into_iter(), |record: &StrippedRecord| {
        warn_sparsify_times(&input_dir, record);
    });

    let parallel = run_pool(config, shutdown, &bar, &results, checked);

    let outcomes = results.into_inner().expect("sparsify results lock");
    for outcome in &outcomes {
        match outcome {
            SparseOutcome::Ok(id) => db.mark_sparsified_sparse(*id)?,
            SparseOutcome::Err(id) => db.mark_sparsified_error(*id)?,
        }
    }


    match parallel {
        Ok(()) => {
            bar.finish("sparsify complete");
            sanity_no_deduped(db)?;
            tracing::info!(
                ok = outcomes.iter().filter(|o| matches!(o, SparseOutcome::Ok(_))).count(),
                err = outcomes.iter().filter(|o| matches!(o, SparseOutcome::Err(_))).count(),
                "sparsify complete"
            );
            Ok(())
        }
        Err(Error::Interrupted) => {
            bar.abandon();
            tracing::warn!(
                saved = outcomes.len(),
                "sparsify interrupted; completed files saved"
            );
            Err(Error::Interrupted)
        }
        Err(e) => Err(e),
    }
}

/// Run the rayon pool over sparsify candidates and return its result.
fn run_pool(
    config: &Config,
    shutdown: &Shutdown,
    bar: &CountProgress,
    results: &Mutex<Vec<SparseOutcome>>,
    checked: impl Iterator<Item = StrippedRecord> + Send,
) -> Result<()> {
    let pool = ThreadPoolBuilder::new()
        .num_threads(config.jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    let input_dir = config.input_dir.clone();
    let stage_dir = config.stage_dir().clone();
    let page_size = config.page_size;
    let shutdown_workers = shutdown.clone();

    let parallel = pool.install(|| {
        checked.par_bridge().try_for_each(|record| {
            shutdown_workers.check_between_files()?;

            let src = input_dir.join(&record.rel_path);
            let name = record.sparse_member_name().expect(
                "Invariant: sparsify candidates are self-canonical files with sha1",
            );
            let dst = stage_dir.join(name);
            let tmp = TempSparseFile::new(dst);

            let copy_result =
                sparse_copy_with_progress(&src, tmp.path(), page_size, |_, _, _| {
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
                    bar.inc(1);
                    Ok(())
                }
                Err(Error::Interrupted) => {
                    drop(tmp);
                    Err(Error::Interrupted)
                }
                Err(_) => {
                    drop(tmp);
                    results
                        .lock()
                        .expect("sparsify results lock poisoned")
                        .push(SparseOutcome::Err(record.id));
                    bar.inc(1);
                    Ok(())
                }
            }
        })
    });
    parallel
}

/// Sanity check that we have files left in the dedup phase.
fn sanity_no_deduped(db: &Database) -> Result<()> {
    let leftover = db.count_files_in_phase(FilePhase::Deduped)?;
    // TODO: This should be some InvariantError or Similar or a panic.
    if leftover != 0 {
        return Err(Error::Config(format!(
            "sparsify finished with {leftover} file(s) still in deduped (expected 0)"
        )));
    }
    Ok(())
}
