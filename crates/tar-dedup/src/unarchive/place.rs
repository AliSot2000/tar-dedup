//! Place: copy/link cached payloads to final output paths.

use crate::config::ExtractConfig;
use crate::db::Database;
use crate::db::types::{FileId, FilePhase, FileRecord};
use crate::error::{Error, Result};
use crate::progress::ByteProgress;
use crate::shutdown::Shutdown;
use filetime::{FileTime, set_file_mtime};
use nix::NixPath;
use path_clean::PathClean;
use rayon::prelude::*;
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::cmp::min;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

const BATCH_SIZE: u64 = 10_000;

// TODO Logging
// TODO Progress

pub fn run2(config: &ExtractConfig, db: &Database, shutdown: &Shutdown) -> Result<()> {
    let files: Vec<FileRecord> = db.list_files_to_restore()?;
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let progress = ByteProgress::new("extract", total_bytes);

    eprintln!(
        "extract: materializing {} file(s) under {}",
        files.len(),
        config.paths.extraction_root().display()
    );

    for record in files {
        shutdown.check_between_files()?;

        let tar_name = record
            .tar_member_name()
            .expect("Invariant Error: FileRecord without sha1 found!");
        let cache_path = config.paths.extract_cache_dir().join(&tar_name);
        if !cache_path.is_file() {
            return Err(Error::Config(format!(
                "missing cached tar member `{tar_name}` for {}",
                record.abs_path.display()
            )));
        }

        //let dest = safe_output_path(config.paths.extraction_root(), &record.abs_path)?;
        // if let Some(parent) = dest.parent() {
        //     fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        // }

        progress.set_file("extract", &record.abs_path);
        //fs::copy(&cache_path, &dest).map_err(|e| Error::io(&dest, e))?;
        // Lightweight mtime/owner until the permissions stage owns full metadata restore.
        //apply_basic_metadata(config, &record, &dest)?;
        db.mark_file_phase(record.id, FilePhase::AtDestination)?;
        progress.inc(record.size);
    }

    progress.finish("extract place complete");
    Ok(())
}

pub fn run(config: &ExtractConfig, db: &Database, shutdown: &Shutdown) -> Result<()> {
    // Step 1.1 empty out source root prior to starting extraction.
    if config.placement.clean_target {
        assert!(!config.placement.no_create_dir,
                "INVARIANT ERROR: clean_target => no_create_dir is false");
        fs::remove_dir_all(config.paths.extraction_root())?;
        fs::create_dir_all(config.paths.extraction_root())?;
    }

    // Step 1.2 Create directoris if required
    if !config.placement.no_create_dir {
        prepare_extraction_dir(&db, &config, shutdown)?;
    }

    // Step 2, move the canonical files into place for link_tree
    if config.placement.link_tree {
        tracing::info!("Moving canonical file in place for link tree...");
        // TODO move that shit
    }

    Ok(())
}

/// Function walks all the extraction directories, ensures there are no symlink on the path if
/// selected, and errors out or r&r any given path entry that was not dir. If path segment does not
/// exist, path es created.
/// PRECONDITION: no_create_dir is false.
pub fn prepare_extraction_dir(db: &Database, config: &ExtractConfig, shutdown: &Shutdown)
    -> Result<()> {
    debug_assert!(config.paths.extraction_root().is_absolute(),
                  "INVARIANT ERROR: extraction root is not absolute");
    // Prepare helper table
    prepare_dir_look_up_table(&db, &config, &shutdown)?;

    // Create dir in relative mode.
    if !config.placement.absolute_names {
        let mut last_source_id = 0i64;
        loop {
            let sources = db.list_sources(
                Some(true), Some(last_source_id), BATCH_SIZE)?;
            if sources.is_empty() { break }
            last_source_id = sources.last().unwrap().id;

            for source in sources {
                let (checked_prefix, _stripped) = strip_leading_up(
                    &source.original_path.clean());
                let source_base_dir = config.paths.extraction_root().join(checked_prefix);
                let mut checked_path = source_base_dir.clone();

                let mut last_dir = FileId(0);
                loop {
                    let dirs = db.list_directories_from_prep(
                        Some(last_dir), BATCH_SIZE, Some(source.id))?;
                    if dirs.is_empty() { break };
                    last_dir = dirs.last().unwrap().id;

                    for dir in dirs {
                        shutdown.check_in_flight()?;
                        let rel_stem = dir.abs_path.strip_prefix(&source.abs_path)
                            .expect("Same Source => Same abs_path prefix");
                        let target = source_base_dir.join(&rel_stem);
                        build_path(&config, &mut checked_path, &target)?;
                    }
                }
            }
        }
    // Create directories in absolute mode.
    } else {
        let mut last_dir = FileId(0);
        let mut previous_check = PathBuf::new();
        loop {
            let dirs = db.list_directories_from_prep(
                Some(last_dir), BATCH_SIZE, None)?;
            if dirs.is_empty() { break }
            last_dir = dirs.last().unwrap().id;

            for dir in dirs {
                shutdown.check_in_flight()?;
                debug_assert!(dir.abs_path.is_absolute(),
                              "INVARIANT ERROR: target_dir path is not absolute");

                let target_dir = config.paths.extraction_root().join(
                    dir.abs_path.strip_prefix("/").unwrap());
                build_path(config, &mut previous_check, &target_dir)?;
            }
        }
    }
    db.drop_prep_ancestor_table()?;
    Ok(())
}

/// Strip leading ../ in relative paths s.t. they do not escape from the extraction target.
fn strip_leading_up(path: &Path) -> (PathBuf, u64) {
    let mut components = path.components().peekable();
    let mut ups = 0u64;
    while matches!(components.peek(), Some(Component::ParentDir)) {
        components.next();
        ups += 1;
    }
    let mut out = PathBuf::new();
    for comp in components {
        if let Component::Normal(name) = comp {
            out.push(name);
        }
    }
    (out, ups)
}

/// Build a given directory path for later extraction.
/// PRECONDITION: Calling function must ensure no_create_dir is false
pub fn build_path(config: &ExtractConfig, already_checked: &mut PathBuf, target: &Path) -> Result<()>  {
    debug_assert!(!config.placement.no_create_dir,
                  "INVARIANT ERROR: build_path may not be called with no_create_dir");
    let mut prefix = PathBuf::new();
    let mut start = 0u64;
    let iter = already_checked
        .components()
        .zip(target.components())
        .enumerate();

    // Check known good prefix
    for (num, (c_comp, t_comp)) in iter {
        if c_comp == t_comp {
           prefix.push(t_comp)
        } else {
            start = num as u64;
            break
        }
    }
    assert!(prefix.len() >= config.paths.extraction_root().len(), "target not within extract dir");

    // Walk new prefix
    for (num, component) in target.components().enumerate() {
        if (num as u64) < start { continue }
        prefix.push(component);
        let printable = prefix.display();

        // Symlink
        if prefix.exists() && prefix.is_symlink() && !config.placement.keep_dir_symlink {
            fs::remove_file(&prefix)?;
            fs::create_dir_all(&prefix)?;
            tracing::info!("Replaced symlink with dir at path: {printable}");
            continue
        }

        // Some but no dir
        if prefix.exists() && !prefix.is_dir() {
            if config.placement.remove_and_replace {
                fs::remove_file(&prefix)?;
                fs::create_dir_all(&prefix)?;
                tracing::info!("Replaced non-dir with dir at path: {printable}");
                continue
            } else {
                return Err(Error::Config(
                    format!("Encountered existing non-directory path at extraction \
                             location where directory was needed: {printable}")));
            }
        }

        // Does not exist
        if !prefix.exists() {
            // INFO: No further checks: no_create_dir is false
            fs::create_dir_all(&prefix)?;
            tracing::info!("Replaced non-dir with dir at path: {printable}");
            continue;
        }

        // By exclusion principle -> dir or symlink with keep_dir_symlink
        assert!(prefix.exists()
                && (prefix.is_dir()
                || (prefix.is_symlink() && config.placement.keep_dir_symlink)),
                "INVARIANT ERROR: Directory should be created or error raised.")
    }

    *already_checked = target.to_path_buf();
    Ok(())
}

/// Resolve all ancestor paths in bottom up order
/// Example: /path/to/dir will become
///     /path/to
///     /path
///     /
fn build_ancestors(path: &Path) -> Vec<PathBuf> {
    let mut res = Vec::new();
    let mut wp = path;

    loop {
        let p = wp.parent();
        match p {
            None => break,
            Some(p) => { res.push(p.to_path_buf()); wp = p; }
        }
    }
    res
}

pub struct MaterializedRelLeaf {
    pub id: FileId,
    pub abs_path: PathBuf,
    pub source_prefix: PathBuf,
    pub source_id: i64,
}

/// Build a lookup table for all directories that need to be created based on the included leaves
/// (non-directory entries)
fn prepare_dir_look_up_table(db: &Database, config: &ExtractConfig, shutdown: &Shutdown)
    -> Result<()> {
    let pool = ThreadPoolBuilder::new()
        .num_threads(config.process.jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    let shutdown = shutdown.clone();

    // Create dir in relative mode.
    if !config.placement.absolute_names {
        let results: Mutex<Vec<Vec<(PathBuf, i64)>>> = Mutex::new(Vec::new());
        let mut last_source_id = 0i64;
        let mut source_leaf_vec: Vec<MaterializedRelLeaf> = Vec::new();

        loop {
            let sources = db.list_sources(
                Some(true), Some(last_source_id), BATCH_SIZE)?;
            if sources.is_empty() { break; }
            last_source_id = sources.last().unwrap().id;

            for source in sources {
                let mut last_leaf_id = FileId(0);
                loop {
                    let leaves = db.list_materialized_leaves(
                        Some(last_leaf_id),
                        min(BATCH_SIZE, BATCH_SIZE - (source_leaf_vec.len() as u64)),
                        Some(source.id)
                    )?;
                    if leaves.is_empty() { break }
                    last_leaf_id = leaves.last().expect(
                        "INVARIANT ERROR: At lest one element expected").id;

                    // Construct structs with the full data so the parallel processes are independent.
                    source_leaf_vec.extend(leaves.iter().map(|l| {
                        MaterializedRelLeaf {
                            id: l.id,
                            abs_path: l.abs_path.clone(),
                            source_prefix: source.abs_path.clone(),
                            source_id: source.id,
                        }
                    }));

                    // Parallel process if enough values present
                    if (source_leaf_vec.len() as u64) == BATCH_SIZE {
                        handle_pool_rel(&db, &shutdown, &mut source_leaf_vec, &results, &pool)?
                    }
                }
            }
        }
        // Last parallel call to empty
        if !source_leaf_vec.is_empty() {
            handle_pool_rel(&db, &shutdown, &mut source_leaf_vec, &results, &pool)?
        }

    // Create directories in absolute mode.
    } else {
        let results: Mutex<Vec<Vec<PathBuf>>> = Mutex::new(Vec::new());
        let mut last_dir = FileId(0);
        loop {
            let dirs = db.list_materialized_leaves(Some(last_dir), BATCH_SIZE, None)?;
            if dirs.is_empty() { break }
            last_dir = dirs.last().unwrap().id;

            let _ = handle_parallel_res(pool.install(|| {
                dirs.par_iter().try_for_each(|record| -> Result<()> {
                    shutdown.check_between_files()?;

                    let ancestors = build_ancestors(&record.abs_path);
                    results.lock().expect("prepare dir lock poisoned").push(ancestors);
                    Ok(())
                })
            }), &shutdown)?;

            let _future_vec: Vec<Vec<PathBuf>> = Vec::new();
            let ancestors = std::mem::replace(
                &mut *results.lock().expect("hash results lock"),
                _future_vec);

            let mut flat_ancestors = Vec::new();
            flat_ancestors.extend(ancestors.into_iter().flatten());
            db.insert_prep_ancestors_abs(&flat_ancestors)?;
        }
    }
    db.link_prep_ancestor_dir_ids()?;
    Ok(())
}

/// Handle the process of computing the ancestors for the rel branch.
/// After ancestors are computed, ancestors are subsequently added to the database and
/// the source_leaf_vec cleared.
/// Code called twice hence separate function
fn handle_pool_rel(
    db: &Database,
    shutdown: &Shutdown,

    source_leaf_vec: &mut Vec<MaterializedRelLeaf>,
    results: &Mutex<Vec<Vec<(PathBuf, i64)>>>,
    pool: &ThreadPool) -> Result<()> {
    debug_assert!(!source_leaf_vec.is_empty(),
                  "INVARIANT ERROR: source_leaf_vec must not be empty");

    // Do parallel processing
    let _ = handle_parallel_res(pool.install(|| {
        source_leaf_vec.par_iter().try_for_each(|record| -> Result<()> {
            shutdown.check_between_files()?;

            let ancestors = build_ancestors(&record.abs_path);
            let pref_ancestors: Vec<(PathBuf, i64)> =
                ancestors
                    .into_iter()
                    .filter(|a| a.starts_with(&record.source_prefix))
                    .map(|p| (p, record.source_id))
                    .collect();
            results
                .lock()
                .expect("prepare dir lock poisoned")
                .push(pref_ancestors);
            Ok(())
        })
    }), &shutdown);

    // Get results
    let _future_vec: Vec<Vec<(PathBuf, i64)>> = Vec::new();
    let ancestors = std::mem::replace(
        &mut *results.lock().expect("hash results lock"),
        _future_vec);

    // Postprocess and store the results
    let mut flat_ancestors = Vec::new();
    flat_ancestors.extend(ancestors.into_iter().flatten());
    db.insert_prep_ancestors_rel(&flat_ancestors)?;
    source_leaf_vec.clear();
    Ok(())

}

fn handle_parallel_res(res: Result<()>, shutdown: &Shutdown) -> Result<()> {
    match res {
        Ok(()) => Ok(()),
        Err(Error::Interrupted) => {
            let op = if shutdown.is_force() {"aborted"} else {"halted"};
            tracing::info!("Place Phase {op}.");
            Err(Error::Interrupted)
        },
        Err(e) => Err(e),
    }
}

pub fn warn_catalog_uncertainty(db: &Database) -> Result<()> {
    let unconfirmed = db.count_unconfirmed_extracted()?;
    if unconfirmed > 0 {
        tracing::warn!(
            "{unconfirmed} extracted file(s) were never promoted to `unarchived` \
             (archive may be incomplete or interrupted)"
        );
    }
    Ok(())
}

fn apply_basic_metadata(
    config: &ExtractConfig,
    record: &FileRecord,
    dest: &Path,
) -> Result<()> {
    if let Some(mtime) = record.mtime {
        let ft = FileTime::from_unix_time(mtime.timestamp(), mtime.timestamp_subsec_nanos());
        let _ = set_file_mtime(dest, ft);
    }

    #[cfg(unix)]
    if config.attributes.restore_owner {
        if let (Some(uid), Some(gid)) = (record.uid, record.gid) {
            use std::os::unix::fs::chown;
            if chown(dest, Some(uid), Some(gid)).is_err() {
                tracing::warn!(path = %dest.display(), "chown failed (need root?)");
            }
        }
    }

    Ok(())
}

fn safe_output_path(output_dir: &Path, rel_path: &Path) -> Result<PathBuf> {
    if rel_path.is_absolute() {
        return Err(Error::Config(format!(
            "absolute path in archive catalog: {}",
            rel_path.display()
        )));
    }
    for component in rel_path.components() {
        if matches!(component, Component::ParentDir) {
            return Err(Error::Config(format!(
                "path escapes output directory: {}",
                rel_path.display()
            )));
        }
    }
    Ok(output_dir.join(rel_path))
}
