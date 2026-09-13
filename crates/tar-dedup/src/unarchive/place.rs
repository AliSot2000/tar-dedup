//! Place: copy/link cached payloads to final output paths.

use crate::cli::{ConflictPolicy, HardLinkGrouping};
use crate::common::files;
use crate::config::{ExtractConfig, ExtractPipelinePhase};
use crate::db::Database;
use crate::db::flags::{ErrorFlags, FileFlag, OutTreeFlag, OutTreeFlags};
#[warn(unused_imports)] // LinkType needed for linking back on windows.
use crate::db::types::{FileId, FileRecord, FileType, NewOutTreeRow, OutTreeId, OutTreeRecord, StrippedRecord};
use crate::db::{ErrorPhase, Recorder};
use crate::error::{Error, FileStatError, Result};
use crate::shutdown::Shutdown;
use nix::NixPath;
use nix::libc::makedev;
use nix::sys::stat::{Mode, SFlag, mknod};
use path_clean::PathClean;
use rayon::ThreadPoolBuilder;
use rayon::prelude::*;
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::{fs, io};

const BATCH_SIZE: u64 = 10_000;
const ERROR_PHASE: ErrorPhase = ErrorPhase::Extract(ExtractPipelinePhase::Place);

// TODO
//  Logging
//  Progress
//  Rethink when we are pub and when private
pub fn run(config: &ExtractConfig, db: &Database, shutdown: &Shutdown) -> Result<()> {
    let mut recorder = Recorder::new(db, !config.process.no_errors);
    if !db.out_tree_is_built()? {
        populate_out_tree(db, config, shutdown)?;
    }
    let capture_error = |rec: &mut Recorder, path: &PathBuf, iof: fn(&Path)
        -> io::Result<()>| {
        match iof(&path) {
            Ok(_) => Ok(()),
            Err(e) => {
                rec.record_session(
                    ERROR_PHASE,
                    FileStatError::io(&path, io::Error::new(e.kind(), e.to_string())),
                    ErrorFlags::default(),
                );
                return Err(Error::io(&path, e));
            }
        }
    };

    // Step 1.1 empty out source root prior to starting extraction.
    if config.placement.clean_target && config.placement.one_top_level.is_some() {
        assert!(!config.placement.no_create_dir,
                "INVARIANT ERROR: clean_target => no_create_dir is false");
        let root = config.paths.extraction_root();
        capture_error(&mut recorder, &root.to_path_buf(), |p| fs::remove_dir_all(p))?;
        capture_error(&mut recorder, &root.to_path_buf(), |p| fs::create_dir_all(p))?;
    }
    // Step 1.2 Create directoris if required
    if !config.placement.no_create_dir && !db.dir_tree_is_built()? {
        prepare_extraction_dir(&db, &config, shutdown, &mut recorder)?;
    }

    // Step 2, move the canonical files into place for link_tree
    if config.placement.link_tree {
        tracing::info!("Moving canonical file in place for link tree...");
        copy_canonicals_to_source(&config, &db, &shutdown, &mut recorder)?;
        // INFO: For linking, we ignore the canonical_id
        link_into_place(&config, &db, &shutdown, &mut recorder)?;
    } else {
        // Step 2, compute hardlink canonicals in the extraction location, then
        // first copy files, then hardlink, then create other types
        // (symlinks, char-dev, block-dev, FIFO)
        prepare_hardlink_canonicals(&config, &db)?;
        let (ac, mc, ah, mh, ao, mo) = status_message_rebuilding(
            &config, &db)?;
        materialize_files(&config, &db, &shutdown, &mut recorder)?;
        materialize_hardlinks(&config, &db, &shutdown, &mut recorder)?;
        materialize_others(&config, &db, &shutdown, &mut recorder)?;
    }
    recorder.flush()?;
    let (placed, ref_linked, conflict, removed, errored, skipped) =
        db.apply_flags_to_files()?;
    tracing::info!(
        "Updated File Table:
        {placed} of entries placed,
        {ref_linked} of entries were copied using reflink,
        {errored} of entries which encountered an error,
        {skipped} of entries skipped due to a placement conflict."
    );
    // TODO push the files records to the next phase
    if !config.process.cleanup.keep_stage {
        let cache_dir = config.paths.extract_cache_dir();
        match capture_error(
            &mut recorder, &cache_dir.to_path_buf(), |p| fs::remove_dir_all(p)) {
            Ok(()) => (),
            Err(e) => {
                tracing::warn!("Failed to clean up stage directors '{}' with error {}",
                    cache_dir.display(), e);
                // TODO fail fast.
            }
        }
    }

    recorder.flush()?;
        Ok(())
}

// -------------------------------------------------------------------------------------------------
// Stage functions
// -------------------------------------------------------------------------------------------------

/// Placement contract:
/// We denote the root extraction dir eith `<ed>` e.g. `/home/user/Desktop/archive`
///
/// If absolute paths are given or absolute_names is true, the following happens
/// `<ed>/var/docker/cache/...`
/// Effectively anything that was under `/` on the scanned system now lands in `<ed>/`
///
/// Relative downwards paths are also mapped directly with a relative prevfix.
/// e.g. `--input-dir ./a/b/c` will be mapped to `<ed>/a/b/c`
/// current dir will also map to root so `--input-dir ./` is `<ed>/`
/// paths that only move up will also convert to extraction root, so
/// `../` or `../../` (or any number of parent dir) will all map to `<ed>/`
///
/// Relative paths up and down again map to the downwards prefix.
/// `../../other/directory` turns into `<ed>/other/directory`
///
/// If multiple files map to the same directory, the tool will not complain and simply the first
/// entry to extract to it, will own the path.
pub fn populate_out_tree(db: &Database, config: &ExtractConfig, shutdown: &Shutdown) -> Result<()> {
    debug_assert!(config.paths.extraction_root().is_absolute(),
                  "INVARIANT ERROR: extraction root is not absolute");
    debug_assert!(!db.out_tree_is_built()?, "PRECONDITION FAILED: out tree built");

    if config.placement.absolute_names {
        populate_out_tree_abs(db, config, shutdown)?;
    } else {
        populate_out_tree_rel(db, config, shutdown)?;
    }

    ensure_parent(db)?;
    db.set_out_tree_built()?;
    Ok(())
}

/// Function walks all the extraction directories, ensures there are no symlink on the path if
/// selected, and errors out or r&r any given path entry that was not dir. If path segment does not
/// exist, path es created.
/// PRECONDITION: no_create_dir is false.
pub fn prepare_extraction_dir(
    db: &Database,
    config: &ExtractConfig,
    shutdown: &Shutdown,
    recorder: &mut Recorder,
) -> Result<()> {
    // TODO info that this process cannot be gracefully interrupted.
    debug_assert!(config.paths.extraction_root().is_absolute(),
                  "INVARIANT ERROR: extraction root is not absolute");
    debug_assert!(db.out_tree_is_built().expect("out_tree meta"),
                  "PRECONDITION FAILED: OutTree must be built to run this function");
    debug_assert!(!db.dir_tree_is_built().expect("dir_tree meta"),
                  "PRECONDITION FAILED: Only run if the dir tree is not built yet");

    let mut last_id = OutTreeId(0);
    let mut already_checked = config.paths.extraction_root().to_path_buf();
    loop {
        let dirs = db.list_out_tree(last_id, BATCH_SIZE, None, Some(true))?;
        if dirs.is_empty() { break }
        last_id = dirs.last().expect("PRECONDITION FAILED: Expected at least one entry").id;

        for dir in dirs {
            shutdown.check_in_flight()?;
            build_path(&config, &mut already_checked, &dir.abs_path, dir.id,
                       recorder)?;
        }
    }
    db.set_dir_tree_built()?;
    Ok(())
}

/// Function copies all extracted canonical files to the extraction destination and
pub fn copy_canonicals_to_source(
    config: &ExtractConfig,
    db: &Database,
    shutdown: &Shutdown,
    recorder: &mut Recorder,
) -> Result<()> {
    let dir_name = match &config.placement.link_source {
        None => PathBuf::from(".sources"),
        Some(v)  => v.to_path_buf(),
    };
    let base_dir = config.paths.extraction_root().join(dir_name);
    let mk_res = fs::create_dir_all(&base_dir);
    match mk_res {
        Ok(_) => (),
        Err(e) => {
            let err = Error::io(&base_dir, e);
            recorder.record_session(
                ERROR_PHASE, err.to_file_stat(Some(&base_dir)), ErrorFlags::default(),
            );
            return Err(err);
        }
    }

    let results: Mutex<Vec<std::result::Result<(FileId, bool), (FileId, Error)>>> =
        Mutex::new(Vec::new());
    let mut last_id = FileId(0);
    let pool = ThreadPoolBuilder::new()
        .num_threads(config.process.jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    loop {
        shutdown.check_in_flight()?;
        let to_copy = db.list_canonical_files_for_move(
            true, last_id, BATCH_SIZE)?;
        if to_copy.is_empty() { break }
        last_id = to_copy.last().expect("PRECONDITION FAILED: Not Empty").id;

        // Parallel File Move
        let parallel = pool.install(|| {
            to_copy.par_iter().try_for_each(|record| -> Result<()> {
                let cid = record.content_id().expect("Content id existed, when extracting.");
                let src = config.paths.extract_cache_dir().join(&cid.0);
                let dst = base_dir.join(&cid.0);
                let res = copy_single_file(
                    record.id, &src, &dst, &shutdown, config.placement.no_reflink
                );
                if !config.process.cleanup.keep_stage {
                    let _ = fs::remove_file(dst);
                }
                results.lock().expect("Canonial File Copy Lock poisoned").push(res);
                Ok(())
            })
        });

        // Check Pool Result
        match parallel {
            Ok(()) => (),
            Err(Error::Interrupted) => (), // Exit
            Err(e) => return Err(e)
        }

        // Get the results
        let new_res = Vec::new();
        let copied = std::mem::replace(
            &mut *results.lock().expect("hash results lock"),
            new_res);

        for result in copied {
            match result {
                Err((id, err)) => {
                    // TODO handle error
                    recorder.record_file(
                        id,
                        ERROR_PHASE,
                        err.to_file_stat(None),
                        ErrorFlags::default(),
                    );
                    db.set_file_flag(id, FileFlag::ErrorWhilePlacing, true)?;
                }
                Ok((id, is_copy)) => {
                    db.set_file_flag(id, FileFlag::AtLinkSource, true)?;
                    db.set_file_flag(id, FileFlag::UsedRefLink, !is_copy)?;
                }
            }
        }
        recorder.flush()?;
    }
    Ok(())
}

/// Function iterates through all entries in the out tree which are not dirs and not unknown builds
/// the tree.
/// Files are linked to the link source and all others links, fifo, char dev, block dev are created,
/// sockets noted but cannot be created
pub fn link_into_place(config: &ExtractConfig, db: &Database, shutdown: &Shutdown,
                       recorder: &mut Recorder) -> Result<()> {
    let dir_name = match &config.placement.link_source {
        None => PathBuf::from(".sources"),
        Some(v)  => v.to_path_buf(),
    };
    let base_dir = config.paths.extraction_root().join(&dir_name);
    let results = Mutex::new(Vec::new());
    let pool = ThreadPoolBuilder::new()
        .num_threads(config.process.jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;
    let shutdown = shutdown.clone();
    loop {
        shutdown.check_between_files()?;
        let entries: Vec<(FileRecord, OutTreeRecord)> = db.list_out_tree_for_linking(
            BATCH_SIZE, true)?;
        if entries.is_empty() { break }


        let parallel = pool.install(|| {
            entries.par_iter().try_for_each(
                |(canonical, out)| -> Result<()> {
                    shutdown.check_between_files()?;
                    let result = if matches!(canonical.ftype, FileType::File) {
                        let content_id = canonical
                            .content_id()
                            .expect("PRECONDITION: Moved successfully, content_id must exist")
                            .0;
                        // Compute the target for link
                        let link_target = if config.placement.absolute_links
                            || config.placement.use_hard_links {
                            base_dir.join(content_id)
                        } else {
                            let up = relative_pardirs_to_dir(
                                config.paths.extraction_root(), &out.abs_path);
                            up.join(&dir_name).join(content_id)
                        };
                        if out.abs_path.exists() {
                            return Err(Error::Config(format!(
                                "Found existing path {}. Link Tree must be empty.",
                                out.abs_path.display())))
                        }

                        // Actually build the link
                        let base_res = if config.placement.use_hard_links {
                            fs::hard_link(link_target, &out.abs_path)
                        } else {
                            // TODO verify that this works
                            #[cfg(unix)]
                            {
                                std::os::unix::fs::symlink(link_target, &out.abs_path)
                            }
                            #[cfg(windows)]
                            {
                                std::os::windows::fs::symlink_file(link_target, &out.abs_path)
                            }
                        };
                        match base_res {
                            Ok(_) => Ok(()),
                            Err(e) => Err(
                                FileStatError::Io {path: out.abs_path.to_path_buf(), source:e}),
                        }
                    } else {
                        build_other(&canonical, &out, config.placement.recreate_none_file_entries)
                    };
                    results
                        .lock()
                        .expect("Result lock for link in place poisoned")
                        .push((out.id, result.err()));
                    Ok(())
                })
        });

        // Check Pool Result
        match parallel {
            Ok(()) => (),
            Err(Error::Interrupted) => (), // Exit
            Err(e) => return Err(e)
        }

        // Get the results
        let new_res = Vec::new();
        let linked = std::mem::replace(
            &mut *results.lock().expect("hash results lock"),
            new_res);

        // Apply results to db
        for (id, err) in linked {
            match err {
                None => {
                    let _ = db.set_out_tree_flag(id, OutTreeFlag::Placed, true)?;
                }
                Some(FileStatError::Io{path: p, source: e}) => {
                    let _ = db.set_out_tree_flag(id, OutTreeFlag::ErrorWhilePlace, true);
                    tracing::error!("Failed to create link: {} with error: {}", p.display(), e);
                    recorder.record_out_tree(
                        id,
                        ERROR_PHASE,
                        FileStatError::Io {
                            path: p.clone(),
                            source: io::Error::new(e.kind(), e.to_string()),
                        },
                        ErrorFlags::default(),
                    );
                }
                Some(FileStatError::Nix{path: p, source: e}) => {
                    let _ = db.set_out_tree_flag(id, OutTreeFlag::ErrorWhilePlace, true);
                    tracing::error!("Failed to create link: {} with error: {}", p.display(), e);
                    recorder.record_out_tree(
                        id,
                        ERROR_PHASE,
                        FileStatError::Nix { path: p.clone(), source: e },
                        ErrorFlags::default(),
                    );
                }
                _ => panic!(
                    "INVARIANT FAILED: link_into_place should only produce Io and Nix Errors.")
            }
        }
        recorder.flush()?;
    }
    Ok(())
}

/// Set the canonical_id of the out_tree.
/// PRECONDITION: This function should ouly be called with link_tree == false
pub fn prepare_hardlink_canonicals(config: &ExtractConfig, db: &Database) -> Result<()> {
    let updates: u64 = match config.placement.hard_link_grouping {
        HardLinkGrouping::None => db.mark_all_canonical()?,
        HardLinkGrouping::Global => db.mark_global_canonical()?,
        HardLinkGrouping::Source => {
            let mut last_id = 0i64;
            let mut sum = 0u64;
            loop {
                let sources = db.list_sources(None, last_id, BATCH_SIZE)?;
                if sources.is_empty() { break }
                last_id = sources
                    .last()
                    .expect("PRECONDITION FAILED: At least one element expected").id;

                for source in sources {
                    sum += db.mark_source_canonical(source.id)?;
                }
            }
            sum
        }
    };
    tracing::info!("Number of materialized target {updates}");
    Ok(())
}

/// Iterate through the out_tree and reflink / copy all files into placed which are marked as
/// (hardlink) canonicals. (out_tree.canonical_id = id)
pub fn materialize_files(
    config: &ExtractConfig, db: &Database, shutdown: &Shutdown,
    recorder: &mut Recorder,
) -> Result<()> {
    let mut last_id = OutTreeId(0);

    let cache_dir = config.paths.extract_cache_dir();
    let shutdown = shutdown.clone();
    let no_reflink = config.placement.no_reflink;

    let results: Mutex<Vec<std::result::Result<MaterializeResult<OutTreeId>, (OutTreeId, Error)>>> =
        Mutex::new(Vec::new());
    let pool = ThreadPoolBuilder::new()
        .num_threads(config.process.jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    loop {
        shutdown.check_between_files()?;
        let entries: Vec<(StrippedRecord, OutTreeRecord)> = db.list_out_tree_for_materialization(
            &last_id, BATCH_SIZE)?;
        if entries.is_empty() { break }
        last_id = entries
            .last().expect("PRECONDITION FAILED: at least one element should exist").1.id;

        let parallel = pool.install(|| {
            entries.par_iter().try_for_each(
                |(canonical, target)| -> Result<()> {
                    let id = canonical.content_id().expect(
                        "PRECONDITION FAILED: Enqueued files must have a content_id");
                    let src = cache_dir.join(id.0);

                    let res =
                        match captured_check_path(&config, &src, &canonical) {
                        Err(e) => Err((target.id, Error::FileStat(e))),
                        Ok((false, conflict, removed)) => Ok(MaterializeResult {
                            id: target.id.clone(),
                            placed: false, conflict, removed, used_copy: false
                        }) ,
                        Ok((true, conflict, removed)) => {
                            tracing::info!("Materializing to {}", target.abs_path.display());
                            match copy_single_file(
                                target.id, &src, &target.abs_path, &shutdown, no_reflink) {
                                Err(e) => Err(e),
                                Ok((id, used_copy)) => Ok(MaterializeResult {
                                    id: id.clone(),
                                    placed: true, conflict, removed, used_copy
                                })
                            }
                        }
                    };
                    results.lock().expect("materialize files lock poisoned").push(res);
                    Ok(())
                })
        });

        match parallel {
            Ok(_) => (),
            Err(Error::Interrupted) => (), // INFO: need to finish iteration
            Err(e) => return Err(e),
        }

        // Get the results
        let new_res = Vec::new();
        let copied = std::mem::replace(
            &mut *results.lock().expect("hash results lock"),
            new_res);

        for result in copied {
            match result {
                Err((id, err)) => {
                    match err {
                        Error::Interrupted => return Err(Error::Interrupted),
                        Error::FileStat(e) => {
                            // Record the copy failure against the out_tree row and keep the flag.
                            recorder.record_out_tree(
                                id, ERROR_PHASE, e, ErrorFlags::default(),
                            );
                            db.set_out_tree_flag(id, OutTreeFlag::ErrorWhilePlace, true)?;
                        },
                        other => panic!("INVARIANT FAILED: Only Io and Interrupted errors \
                        expected, got {}", other)

                    }
                }
                Ok(suc) => {
                    if cfg!(debug_assertions) {
                        validate_materialize_result(&suc);
                    }
                    db.set_out_tree_flag(
                        suc.id, OutTreeFlag::Placed, suc.placed)?;
                    db.set_out_tree_flag(
                        suc.id, OutTreeFlag::UsedRefLink, !suc.used_copy)?;
                    db.set_out_tree_flag(
                        suc.id, OutTreeFlag::RemovedPrevious, !suc.removed)?;
                    db.set_out_tree_flag(
                        suc.id, OutTreeFlag::Conflict, suc.conflict)?;
                }
            }
        }
        recorder.flush()?;
    }
    Ok(())
}

#[cfg(debug_assertions)]
fn validate_materialize_result<I>(m: &MaterializeResult<I>) -> () {
    match (m.placed, m.conflict, m.removed, m.used_copy) {
        // No conflict
        (true, false, false, _) => (),
        // Conflict, skip
        (false, true, false, false) => (),
        // Conflict, place
        (true, true, _, _) => (),
        _ => panic!("INVARIANT FAILED: Impossible flag constellation returned from placement ")
    }
}

/// Create all the hardlinks after the copy stage.
/// PRECONDITION: Function must be called after the [`materialize_files`]
pub fn materialize_hardlinks(
    config: &ExtractConfig, db: &Database, shutdown: &Shutdown,
    recorder: &mut Recorder)
    -> Result<()> {
    let mut last_id = OutTreeId(0);

    let shutdown = shutdown.clone();

    let results: Mutex<Vec<std::result::Result<OutTreeId, (OutTreeId, Error)>>> =
        Mutex::new(Vec::new());
    let pool = ThreadPoolBuilder::new()
        .num_threads(config.process.jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    loop {
        shutdown.check_between_files()?;
        let entries: Vec<(OutTreeRecord, OutTreeRecord)> = db.list_out_tree_for_hardlinks(
            &last_id, BATCH_SIZE)?;
        if entries.is_empty() { break }
        last_id = entries
            .last().expect("PRECONDITION FAILED: at least one element should exist").1.id;

        let parallel = pool.install(|| {
            entries.par_iter().try_for_each(
                |(canonical, target)| -> Result<()> {
                    let src = &canonical.abs_path;
                    let dst = &target.abs_path;

                    // TODO handle unlink_first,
                    // Todo handle newer, older

                    let res = match fs::hard_link(src, dst) {
                        Ok(()) => Ok(target.id),
                        Err(e) => Err((target.id, Error::io(dst, e))),
                    };
                    results.lock().expect("materialize files lock poisoned").push(res);
                    Ok(())
                })
        });

        match parallel {
            Ok(_) => (),
            Err(Error::Interrupted) => (), // INFO: need to finish iteration
            Err(e) => return Err(e),
        }

        // Get the results
        let new_res = Vec::new();
        let copied = std::mem::replace(
            &mut *results.lock().expect("hash results lock"),
            new_res);

        for result in copied {
            match result {
                Err((id, err)) => {
                    recorder.record_out_tree(
                        id,
                        ERROR_PHASE,
                        err.to_file_stat(None),
                        ErrorFlags::default(),
                    );
                    db.set_out_tree_flag(id, OutTreeFlag::ErrorWhilePlace, true)?;
                }
                Ok(id) => {
                    db.set_out_tree_flag(id, OutTreeFlag::Placed, true)?;
                    db.set_out_tree_flag(id, OutTreeFlag::IsHardlink, true)?;
                }
            }
        }
        recorder.flush()?;
    }
    Ok(())
}

/// Final step, pass through all the remaining entries which could be materialized:
/// (symlink, fifo, character device, block device, socket)
pub fn materialize_others(
    config: &ExtractConfig, db: &Database, shutdown: &Shutdown,
    recorder: &mut Recorder,
) -> Result<()> {
    let mut last_id = OutTreeId(0);

    let shutdown = shutdown.clone();

    let results: Mutex<Vec<std::result::Result<OutTreeId, (OutTreeId, Error)>>> =
        Mutex::new(Vec::new());
    let pool = ThreadPoolBuilder::new()
        .num_threads(config.process.jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    loop {
        shutdown.check_between_files()?;
        let entries: Vec<(FileRecord, OutTreeRecord)> = db.list_out_tree_others(
            &last_id, BATCH_SIZE)?;
        if entries.is_empty() { break }
        last_id = entries
            .last().expect("PRECONDITION FAILED: at least one element should exist").1.id;

        let parallel = pool.install(|| {
            entries.par_iter().try_for_each(
                |(canonical, target)| -> Result<()> {

                    // TODO handle unlink_first,
                    // Todo handle newer, older
                    let base_res = build_other(
                        &canonical, &target, config.placement.recreate_none_file_entries);

                    let res = match base_res {
                        Ok(()) => Ok(target.id),
                        Err(e) => Err((target.id, Error::FileStat(e)))
                    };

                    results.lock().expect("materialize files lock poisoned").push(res);
                    Ok(())
                })
        });

        match parallel {
            Ok(_) => (),
            Err(Error::Interrupted) => (), // INFO: need to finish iteration
            Err(e) => return Err(e),
        }

        // Get the results
        let new_res = Vec::new();
        let copied = std::mem::replace(
            &mut *results.lock().expect("hash results lock"),
            new_res);
        for result in copied {
            match result {
                Err((_, Error::Interrupted)) => {
                    recorder.flush()?;
                    return Err(Error::Interrupted);
                }
                Err((id, Error::FileStat(fse))) => {
                    recorder.record_out_tree(
                        id,
                        ERROR_PHASE,
                        fse,
                        ErrorFlags::default(),
                    );
                    db.set_out_tree_flag(id, OutTreeFlag::ErrorWhilePlace, true)?;
                }
                Err((id, other)) => {
                    recorder.record_out_tree(
                        id,
                        ERROR_PHASE,
                        other.to_file_stat(None),
                        ErrorFlags::default(),
                    );
                    db.set_out_tree_flag(id, OutTreeFlag::ErrorWhilePlace, true)?;
                }
                Ok(id) => {
                    db.set_out_tree_flag(id, OutTreeFlag::Placed, true)?;
                }
            }
        }
        recorder.flush()?;
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------------
// Stage Subfunctions
// -------------------------------------------------------------------------------------------------


/// Build the out_tree table, if the user selected absolute names for the materialization method.
fn populate_out_tree_abs(db: &Database, config: &ExtractConfig, shutdown: &Shutdown) -> Result<()> {
    debug_assert!(config.placement.absolute_names,
                  "PRECONDITION FAILED: Function builds absolute names");
    let root = config.paths.extraction_root();
    let mut last_id = FileId(0);

    loop {
        shutdown.check_in_flight()?;
        let entries: Vec<StrippedRecord> = db.list_materialized_entries(
            Some(last_id), BATCH_SIZE, None, Some(false))?;
        if entries.is_empty() { break }
        last_id = entries.last().expect("non-empty batch").id;

        // Process the entries
        let processed: Vec<NewOutTreeRow> = build_new_out_tree_rows(
            &entries, &root, None);

        db.insert_out_tree_rows(&processed)?;
        // INFO ref table is left empty since we are working with abs_paths
    }
    Ok(())
}

/// Build the out_tree table, if the user selected no absolute names for the materialization method.
/// Paths are populated on first-come-first-serve basis. I.e. if the user used
/// --no-strict-separation, it is possible that parts of the tree were mapped twice and those names
/// might collide. The sources are selected in ascending order (same order as adding and scanning
/// initially) and their subtree is then materialized at its relative target.
fn populate_out_tree_rel(db: &Database, config: &ExtractConfig, shutdown: &Shutdown) -> Result<()> {
    let root = config.paths.extraction_root();
    let mut last_source_id = 0i64;

    loop {
        let sources = db.list_sources(None, last_source_id, BATCH_SIZE)?;
        if sources.is_empty() {
            break;
        }
        last_source_id = sources.last().expect("non-empty batch").id;

        for source in sources {
            let san_org_path = source.original_path.clean();

            let extraction_base: PathBuf = if san_org_path.is_absolute() {
                // Got root dir, simply return the extract root
                if san_org_path == PathBuf::from("/") {
                    root.to_path_buf()
                } else {
                    let stripped = san_org_path
                        .strip_prefix("/")
                        .expect("Absolute expects / at the beginning");
                    root.join(stripped)
                }
            } else {
                let (cut, _) = strip_leading_up(&san_org_path);
                debug_assert!(!cut.starts_with("/"), "Relative does not expect a / at begin");
                root.join(cut)
            };

            // File Loop
            let mut last_id = FileId(0);
            loop {
                shutdown.check_in_flight()?;
                let entries: Vec<StrippedRecord> = db.list_materialized_entries(
                    Some(last_id), BATCH_SIZE, Some(source.id), Some(false)
                )?;
                if entries.is_empty() { break }
                last_id = entries.last().expect("non-empty batch").id;

                let processed: Vec<NewOutTreeRow> = build_new_out_tree_rows(
                    &entries, &root, Some((&source.abs_path, &extraction_base)));

                let out_ids = db.insert_out_tree_rows(&processed)?;
                let ref_pairs: Vec<(OutTreeId, i64)> = out_ids
                    .iter()
                    .map(|id| { (id.clone(), source.id) })
                    .collect();
                db.insert_ref_out_rows(&ref_pairs)?;
            }
        }
    }
    Ok(())
}

/// Given a vectors of StrippedRecords compute the new OutTreeRows
fn build_new_out_tree_rows(
    entries: &Vec<StrippedRecord>, root: &Path, sources: Option<(&Path, &Path)>)
    -> Vec<NewOutTreeRow> {
    let processed: Vec<NewOutTreeRow> = entries
        .iter()
        .map(|r| {
            let path = catalog_to_target_abs(root, &r.abs_path, sources);
            let is_dir = r.ftype == FileType::Directory;
            let mut of = OutTreeFlags::default();
            of.set(OutTreeFlag::IsDirectory, is_dir);
            NewOutTreeRow {
                abs_path: path.clone(),
                file_id: Some(r.id),
                flags: of,
            }
        })
        .collect();
    processed
}

/// Ensures parent exists for all non-directory rows inside the out_tree. This is needed in case
/// a `--files-fromm` file had a file with `/path/to/not/covered/directory/file.txt`
/// where `/path/to/other/*` is covered by recursive index. Creating file.txt would fail because
/// there's no parent path.
pub fn ensure_parent(db: &Database) -> Result<()> {
    let mut current_parents: HashSet<PathBuf> = HashSet::new();
    let mut parent_rows: Vec<NewOutTreeRow> = Vec::new();
    let mut last_id = OutTreeId(0);
    let mut flags = OutTreeFlags::default();
    flags.set(OutTreeFlag::IsDirectory, true);
    let cref_flags = &flags;

    loop {
        let entries = db.list_out_tree(
            last_id, BATCH_SIZE, None, Some(false))?;
        if entries.is_empty() { break }
        last_id = entries.last().expect("PRECONDITION FAILED: Must have at least one").id;

        for entry in entries {
            let par = entry.abs_path.parent().expect("File needs Parent");
            current_parents.insert(par.to_path_buf());
        }

        for parent in &current_parents {
            parent_rows.push(NewOutTreeRow {
                abs_path: parent.clone(),
                file_id: None,
                flags: cref_flags.clone(),
            })
        }
        db.insert_out_tree_rows(&parent_rows)?;
        // Clear the accumulators before the next run to avoid huge structures in ram.
        current_parents.clear();
        parent_rows.clear();
    }

    Ok(())
}

/// Function recreates all special files it can. Importantly, files, directories and unknown
/// types are not valid file types for the function and will cause a panic
fn build_other(canonical: &FileRecord, out_tree: &OutTreeRecord, try_special: bool)
    -> std::result::Result<(), FileStatError> {
    match canonical.ftype {
        FileType::File => panic!(
            "PRECONDITION ERROR: build_other does not treat files"),
        FileType::Directory => panic!(
            "PRECONDITION ERROR: build_other does not treat directories"),
        FileType::Unknown => panic!(
            "PRECONDITION ERROR: build_other does not treat unknown"),
        FileType::Socket => {
            tracing::info!("Received Socket at {}, skipping", &out_tree.abs_path.display());
            Ok(())
        },
        #[cfg(unix)]
        FileType::Symlink(_) => match &canonical.link_dst {
            None => Ok(()),
            Some(dst) => match std::os::unix::fs::symlink(dst, &out_tree.abs_path) {
                Ok(_) => Ok(()),
                Err(e) => Err(
                    FileStatError::Io{path: out_tree.abs_path.to_path_buf(), source: e})
            }
        },
        #[cfg(windows)]
        FileType::Symlink(LinkType::Directory) => match &canonical.link_dst {
            None => Ok(()),
            Some(dst) => match std::os::windows::fs::symlink_dir(dst, &out_tree.abs_path) {
                Ok(_) => Ok(()),
                Err(e) => Err(FileStatError::Io{path: out_tree.abs_path.to_path_buf(), source: e})
            }
        },
        #[cfg(windows)]
        FileType::Symlink(_) => match &canonical.link_dst {
            None => Ok(()),
            Some(dst) => match std::os::windows::fs::symlink_file(dst, &out_tree.abs_path) {
                Ok(_) => Ok(()),
                Err(e) => Err(FileStatError::Io{path: out_tree.abs_path.to_path_buf(), source: e})
            }
        },
        FileType::FIFO => {
            if !try_special {
                return Ok(())
            }
            match nix::unistd::mkfifo(&out_tree.abs_path, Mode::from_bits_truncate(0o644)) {
                Ok(_) => Ok(()),
                Err(e) => Err(
                    FileStatError::Nix {path: out_tree.abs_path.to_path_buf(), source: e})
            }
        },
        FileType::BlockDevice => {
            if canonical.major.is_none() || canonical.minor.is_none() {
                tracing::error!(
                    "Could not create block device at {}, major and/or minor is missing",
                    out_tree.abs_path.display()
                );
                return Ok(())
            }
            if !try_special {
                return Ok(())
            }
            let dev = makedev(
                canonical.major.unwrap() as u32, canonical.minor.unwrap()  as u32);
            let create_res =  mknod(
                &out_tree.abs_path, SFlag::S_IFBLK, Mode::from_bits_truncate(0o644), dev);
            match create_res {
                Ok(_) => Ok(()),
                Err(e) => Err(
                    FileStatError::Nix {path: out_tree.abs_path.to_path_buf(), source: e})
            }
        },
        FileType::CharacterDevice => {
            if canonical.major.is_none() || canonical.minor.is_none() {
                tracing::error!(
                    "Could not create block device at {}, major and/or minor is missing",
                    out_tree.abs_path.display()
                );
                return Ok(())
            }
            if !try_special {
                return Ok(())
            }
            let dev = makedev(
                canonical.major.unwrap() as u32, canonical.minor.unwrap()  as u32);
            let create_res =  mknod(
                &out_tree.abs_path, SFlag::S_IFCHR, Mode::from_bits_truncate(0o644), dev);
            match create_res {
                Ok(_) => Ok(()),
                Err(e) => Err(
                    FileStatError::Nix {path: out_tree.abs_path.to_path_buf(), source: e})
            }
        }
    }
}

/// Get the number of files, hardlinks and others. Also produce
pub fn status_message_rebuilding(config: &ExtractConfig, db: &Database)
    -> Result<(u64, u64, u64, u64, u64, u64)> {
    let all_canonicals = db.count_out_tree_canonicals(None)?;
    let all_hardlinks = db.count_out_tree_hardlinks(None)?;
    let all_other = db.count_out_tree_others(None)?;
    let materialized_canonicals = db.count_out_tree_canonicals(Some(false))?;
    let materialized_hardlinks = db.count_out_tree_hardlinks(Some(false))?;
    let materialized_other = db.count_out_tree_others(Some(false))?;

    let other_msg = if config.placement.recreate_none_file_entries {
        &format!("{materialized_other} of other entries, {} remaining",
                 all_other - materialized_other)
    } else {
        ""
    };

    tracing::info!("
        Placement Phase:
        {materialized_canonicals} of files already copied, {} remaining.
        {materialized_hardlinks} of files already created, {} remaining.
        {other_msg}",
        all_canonicals - materialized_canonicals,
        all_hardlinks - materialized_hardlinks,
    );
    Ok((all_canonicals,
        materialized_canonicals,
        all_hardlinks,
        materialized_hardlinks,
        all_other,
        materialized_other
    ))
}

// -------------------------------------------------------------------------------------------------
// Util
// -------------------------------------------------------------------------------------------------

/// Result:
/// - first bool is true iff the file can be written to the path
/// - second bool is true iff there was a conflict which resolved in favor of extracting
/// - third bool is true iff there was a file was removed during the prep process.
struct MaterializeResult<I> {
    id: I,
    placed: bool,
    conflict: bool,
    removed: bool,
    used_copy: bool
}

/// Relative path of `..` components from `file`'s parent directory back to `dir`.
///
/// - `dir=/path/to/dir`, `file=/path/to/dir/sub/dir/file.txt` → `../../`
/// - `dir=/path/to/dir`, `file=/path/to/dir/file.txt` → `.`
///
/// Panics if `file` is not under `dir`.
fn relative_pardirs_to_dir(dir: &Path, file: &Path) -> PathBuf {
    debug_assert!(dir.is_absolute(), "dir path must be absolute");
    debug_assert!(file.is_absolute(), "file path must be absolute");
    debug_assert_eq!(dir.clean(), dir, "dir path must be cleaned");
    debug_assert_eq!(file.clean(), file, "dir path must be cleaned");
    assert!(
        file.starts_with(&dir),
        "provided path is not child of target path: {} is not under {}",
        file.display(),
        dir.display()
    );
    let parent = file
        .parent()
        .expect("file path must have a parent directory");
    let below = parent
        .strip_prefix(&dir)
        .expect("parent must be under dir after starts_with check");
    // INFO Check exists but it should already be guaranteed not to exist.
    // for c in below.components() {
    //     if !matches!(c, Component::Normal(_)) {
    //         assert!(false, "Absolute Path contained non-Normal intermediate entry.");
    //     }
    // }
    let depth = below
        .components()
        .count();
    if depth == 0 {
        PathBuf::from(".")
    } else {
        let mut up = String::with_capacity(depth * 3);
        for _ in 0..depth {
            up.push_str("../");
        }
        PathBuf::from(up)
    }
}

/// Copy a single file from a to b. Function implements a shutdown check to avoid long blocking
/// Error contains the Error as well as the file id to link against,
/// Ok contains the id as well as bool which is false if reflink was used and true if copy was used.
fn copy_single_file<ID>(fid: ID, src: &Path, dst: &Path, shutdown: &Shutdown, no_reflink: bool)
    -> std::result::Result<(ID, bool), (ID, Error)> {
    // Attempt to reflink
    if !no_reflink {
        let worked = reflink::reflink(src, dst);
        if worked.is_ok() {
            return Ok((fid, false))
        }
    }

    // Failed, perform sparse copy
    let spc_res = sparse_cp::sparse_copy_with_progress(
        src,
        dst,
        4096,
        |_, _, _| -> Result<()> { shutdown.check_in_flight() }
    );

    // Handle result
    match spc_res {
        Ok(_) => Ok((fid, true)),
        Err(e) => {
            let _ = fs::remove_file(dst);
            Err((fid, e))
        }
    }
}

/// Strip leading ../ in relative paths s.t. they do not escape from the extraction target.
fn strip_leading_up(path: &Path) -> (PathBuf, u64) {
    debug_assert_eq!(path, path.clean(),
                     "INVARIANT ERROR: Function should only work on clean paths");
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
pub fn build_path(config: &ExtractConfig, already_checked: &mut PathBuf, target: &Path,
                  out_tree_id: OutTreeId,
                  recorder: &mut Recorder) -> Result<()>  {
    debug_assert!(!config.placement.no_create_dir,
                  "INVARIANT ERROR: build_path may not be called with no_create_dir");
    let mut prefix = PathBuf::new();
    let mut start = 0u64;
    let mut capture_error = |path: &PathBuf, iof: fn(&Path) -> io::Result<()>| {
        match iof(&path) {
            Ok(_) => Ok(()),
            Err(e) => {
                recorder.record_out_tree(
                    out_tree_id,
                    ERROR_PHASE,
                    FileStatError::io(&path, io::Error::new(e.kind(), e.to_string())),
                    ErrorFlags::default(),
                );
                return Err(Error::io(&path, e));
            }
        }
    };
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
            // INFO: Raise; if fs fails here, almost guaranteed that extraction fails.
            capture_error(&prefix, |p: &Path| fs::remove_file(p))?;
            capture_error(&prefix, |p: &Path| fs::create_dir_all(p))?;
            tracing::info!("Replaced symlink with dir at path: {printable}");
            continue
        }

        // Some but no dir
        if prefix.exists() && !prefix.is_dir() {
            if config.placement.remove_and_replace {
                // INFO: Raise; if fs fails here, almost guaranteed that extraction fails.
                capture_error(&prefix, |p: &Path| fs::remove_file(p))?;
                capture_error(&prefix, |p: &Path| fs::create_dir_all(p))?;
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
            capture_error(&prefix, |p: &Path| fs::create_dir_all(p))?;
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

/// Build the abs path where a given file is extracted to.
fn catalog_to_target_abs(
    extraction_root: &Path,
    catalog_path: &Path,
    relative_component: Option<(&Path, &Path)>,
) -> PathBuf {
    match relative_component {
        None => {
            let rel = catalog_path
                .strip_prefix("/")
                .expect(&format!(
                    "INVARIANT ERROR: Catalogue Path MUST be absolute and start with /, got {}",
                    catalog_path.display()
                ));
            extraction_root.join(rel)
        }
        Some((source_abs, source_base)) => {
            let rel_stem = catalog_path
                .strip_prefix(source_abs)
                .expect("Source rows must start within source root");
            source_base.join(rel_stem)
        }
    }
}
/// Perform the pre-materialize check.
/// First perform the conflict policy check based on mtime, then check the file type, lastly, if
/// required remove previous file.
/// On error, the file on the file system wins.
/// Result:
/// - first bool is true iff the file can be written to the path
/// - second bool is true iff there was a conflict which resolved in favor of extracting
/// - third bool is true iff there was a file was removed during the prep process.
fn captured_check_path(config: &ExtractConfig, tgt: &Path, rec: &StrippedRecord)
                       -> std::result::Result<(bool, bool, bool), FileStatError> {
    if !tgt.exists() {
        return Ok((true, false, false))
    };

    // PRECONDITION: Path exists
    let file_metadata = match tgt.symlink_metadata() {
        Err(e) => {
            return if config.placement.silent_conflicts {
                tracing::info!(
                        "Leaving {}, could not assess conflict due to inaccessible file metadata",
                        tgt.display());
                Ok((false, true, false))
            } else {
                Err(FileStatError::Io { path: tgt.to_path_buf(), source: e })
            }
        }
        Ok(m) => m,
    };

    // Check the times with the conflict policy
    if !matches!(config.placement.conflict_policy, ConflictPolicy::Replace) {
        // mtime of record
        let db_mtime = match rec.mtime {
            None => {
                if !config.placement.silent_conflicts {
                    tracing::info!(
                        "Leaving {}, could not assess conflict due to missing mtime in db",
                        tgt.display());
                }
                return Ok((false, true, false));
            }
            Some(t) => t
        };
        let (res_mtime, _, _) = files::get_file_times(&file_metadata);
        let file_mtime = match res_mtime {
            Err(e) => {
                return if config.placement.silent_conflicts {
                    tracing::info!(
                        "Leaving {}, could not retrieve file mtime",
                    tgt.display());
                    Ok((false, true, false))
                } else {
                    Err(FileStatError::Io { path: tgt.to_path_buf(), source: e })
                }
            }
            Ok(m) => m,
        };
        match config.placement.conflict_policy {
            ConflictPolicy::Replace => (),
            ConflictPolicy::PreferNewer => {
                if db_mtime <= file_mtime {
                    return if !config.placement.silent_conflicts {
                        Err(FileStatError::General {
                            path: Some(tgt.to_path_buf()), message: "File is too old.".to_string()
                        })
                    } else {
                        tracing::info!("Could not extract to {}, file is too old.",  tgt.display());
                        Ok((false, true, false))
                    }
                }
            },
            ConflictPolicy::PreferOlder => {
                if db_mtime >= file_mtime {
                    return if !config.placement.silent_conflicts {
                        Err(FileStatError::General {
                            path: Some(tgt.to_path_buf()), message: "File is new old.".to_string()
                        })
                    } else {
                        tracing::info!("Could not extract to {}, file is too new.",  tgt.display());
                        Ok((false, true, false))
                    }
                }
            }
        }
    }

    // Check if the file types are a match.
    let ftype = match files::determine_file_type(&file_metadata, tgt) {
        Ok(ft) => ft,
        Err((ft, failed_path, e)) => {
            // INFO: Error means ftype unknown. Can proceed, add error regardless
            if config.placement.silent_conflicts {
                tracing::info!("Error while determining file type of {}. Got {}",
                tgt.display(), &e);
            } else {
                return Err(FileStatError::Io{ path: failed_path, source: e })
            };
            ft
        }
    };
    let remove = if ftype != rec.ftype {
        if config.placement.remove_and_replace {
            tracing::info!("File type mismatch, expected {} got {}",
                rec.ftype.as_str(), ftype.as_str());
            true
        } else {
            return if !config.placement.silent_conflicts {
                Err(FileStatError::General {
                    path: Some(tgt.to_path_buf()),
                    message: format!("File type mismatch, expected {} got {}",
                                     rec.ftype.as_str(), ftype.as_str())
                })
            } else {
                tracing::info!("File type mismatch, expected {} got {}",
                    rec.ftype.as_str(), ftype.as_str());
                Ok((false, true, false))
            }
        }
    } else {
        false
    };
    // Remove file, if indicated.
    if remove || config.placement.unlink_first {
        match fs::remove_file(&tgt) {
            Ok(()) => Ok((true, true, true)),
            Err(e) => {
                if config.placement.silent_conflicts {
                    tracing::info!("Failed to remove preexisting path {} with error {}",
                        tgt.display(), e);
                    Ok((false, true, false))
                } else {
                    Err(FileStatError::Io { path: tgt.to_path_buf(), source: e })
                }
            }
        }
    } else {
        Ok((true, true, false))
    }
}