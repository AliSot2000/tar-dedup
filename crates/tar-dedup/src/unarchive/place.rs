//! Place: copy/link cached payloads to final output paths.

use crate::cli::ConflictPolicy;
use crate::common::files;
use crate::common::{batched_loop, batched_stepped_loop, send_receive_loop};
use crate::config::{ExtractConfig, ExtractPipelinePhase};
use crate::db::Database;
use crate::db::flags::{ErrorFlags, OutTreeFlag};
use crate::db::place::{CopyOutcome, MaterializeResult};
use crate::db::types::FilePhase;
#[warn(unused_imports)] // LinkType needed for linking back on windows.
use crate::db::types::{FileId, FileRecord, FileType, OutTreeId, OutTreeRecord, StrippedRecord};
use crate::db::{ErrorPhase, Recorder};
use crate::error::{Error, FileStatError, Result};
use crate::progress::BarKind;
use crate::shutdown::Shutdown;
use crate::unarchive::ExtractRTArgs;
use crossbeam_channel::{Receiver, Sender, bounded};
use indicatif::ProgressBar;
use nix::NixPath;
use nix::libc::makedev;
use nix::sys::stat::{Mode, SFlag, mknod};
use path_clean::PathClean;
use rayon::ThreadPoolBuilder;
use rayon::prelude::*;
use std::mem::take;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::{fs, io};

const BATCH_SIZE: u64 = 10_000;
const ERROR_PHASE: ErrorPhase = ErrorPhase::Extract(ExtractPipelinePhase::Place);

// Producer/consumer pipeline bounds: the input queue takes over the old
// whole-batch pull's memory guard, but workers stream file-by-file so a big
// file no longer stalls every other row's commit and progress.
const WORK_CAPACITY: usize = BATCH_SIZE as usize;
const OUT_CAPACITY: usize = 2 * WORK_CAPACITY;
const FEED_CHUNK: usize = 1_024;                     // rows pulled from the DB per round
const DRAIN_CHUNK: usize = BATCH_SIZE as usize / 2;  // outcomes committed per transaction

// TODO
//  Logging
//  Progress
//  Rethink when we are pub and when private
pub fn run(rt: &ExtractRTArgs) -> Result<()> {
    debug_assert!(rt.db.placement_prologue_done()?,
                  "PRECONDITION FAILED: PlacementPrologue must complete before place");
    let mut recorder = Recorder::new(rt.db, !rt.config.process.no_errors);
    let progress = rt.progress;
    progress.set_phase_total(rt.db.count_out_tree_rows()?);
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
    if rt.config.placement.clean_target && rt.config.placement.one_top_level.is_some() {
        assert!(!rt.config.placement.no_create_dir,
                "INVARIANT ERROR: clean_target => no_create_dir is false");
        let root = rt.config.paths.extraction_root();
        capture_error(&mut recorder, &root.to_path_buf(), |p| fs::remove_dir_all(p))?;
        capture_error(&mut recorder, &root.to_path_buf(), |p| fs::create_dir_all(p))?;
    }
    // Step 1.2 Create directoris if required
    if !rt.config.placement.no_create_dir && !rt.db.dir_tree_is_built()? {
        prepare_extraction_dir(&rt, &mut recorder)?;
    }

    // Step 2, move the canonical files into place for link_tree
    if rt.config.placement.link_tree {
        tracing::info!("Moving canonical file in place for link tree...");
        copy_canonicals_to_source(rt, &mut recorder)?;
        // INFO: For linking, we ignore the canonical_id
        materialize_link_tree(rt, &mut recorder)?;
    } else {
        // Step 2, first copy files, then hardlink, then create other types
        // (symlinks, char-dev, block-dev, FIFO). Canonical election ran in
        // the PlacementPrologue phase.
        let (_ac, _mc, _ah, _mh, _ao, _mo) = status_message_rebuilding(rt)?;
        materialize_files(rt, &mut recorder)?;
        materialize_hardlinks(rt, &mut recorder)?;
        materialize_others(rt, &mut recorder)?;
    }
    let res = rt.db.apply_flags_to_files()?;
    let (placed, ref_linked, conflict, removed, errored, skipped) = res;
    tracing::info!(
        "Updated File Table:
        {placed} of entries placed,
        {ref_linked} of entries were copied using reflink,
        {errored} of entries which encountered an error,
        {skipped} of entries skipped due to a placement conflict.
        {conflict} of entries encountered a conflict
        {removed} of entries that attempted to remove before placeing the extracted file."
    );

    if !rt.config.process.cleanup.keep_stage {
        let cache_dir = rt.config.paths.extract_cache_dir();
        match capture_error(
            &mut recorder,
            &cache_dir.to_path_buf(), |p| fs::remove_dir_all(p)) {
            Ok(()) => (),
            Err(e) => {
                if rt.config.process.fail_fast {
                    return Err(Error::Config(format!(
                        "Failed to clean up stage directors '{}' with error {}",
                        cache_dir.display(), e)))
                } else {
                    tracing::warn!("Failed to clean up stage directors '{}' with error {}",
                    cache_dir.display(), e);
                }
            }
        }
    }
    // INFO: Since all relevant things are stored in out_tree, we cna blanket promote here.
    if !rt.shutdown.is_interrupted() {
        rt.db.global_mark_phase(FilePhase::AtDestination)?;
    }
    recorder.flush()?;
    Ok(())
}

// -------------------------------------------------------------------------------------------------
// Stage functions
// -------------------------------------------------------------------------------------------------

/// Function walks all the extraction directories, ensures there are no symlink on the path if
/// selected, and errors out or r&r any given path entry that was not dir. If path segment does not
/// exist, path es created.
/// PRECONDITION: no_create_dir is false.
pub fn prepare_extraction_dir(
    rt: &ExtractRTArgs,
    recorder: &mut Recorder) -> Result<()> {
    debug_assert!(rt.config.paths.extraction_root().is_absolute(),
                  "INVARIANT ERROR: extraction root is not absolute");
    debug_assert!(rt.db.out_tree_is_built().expect("out_tree meta"),
                  "PRECONDITION FAILED: OutTree must be built to run this function");
    debug_assert!(!rt.db.dir_tree_is_built().expect("dir_tree meta"),
                  "PRECONDITION FAILED: Only run if the dir tree is not built yet");
    tracing::warn!("Building the Directory tree cannot be gracefully interrupted!");

    let mut already_checked = rt.config.paths.extraction_root().to_path_buf();

    batched_stepped_loop(
        BATCH_SIZE,
        || OutTreeId(0),
        |lid, bs| rt.db.list_out_tree(*lid, bs, None, Some(true)),
        |rec| rec.id,
        |dirs| {
            for dir in dirs {
                rt.shutdown.check_in_flight()?;
                build_path(&rt.config, &mut already_checked, &dir.abs_path, dir.id, recorder)?;
                rt.progress.inc_both(1);
            }
            Ok(())
        }
    )?;
    rt.db.set_dir_tree_built()?;
    Ok(())
}

/// Function copies all extracted canonical files to the extraction destination and
pub fn copy_canonicals_to_source(rt: &ExtractRTArgs, recorder: &mut Recorder)
    -> Result<()> {
    let dir_name = match &rt.config.placement.link_source {
        None => PathBuf::from(".sources"),
        Some(v) => v.to_path_buf(),
    };
    let base_dir = rt.config.paths.extraction_root().join(dir_name);
    let mk_res = fs::create_dir_all(&base_dir);
    match mk_res {
        Ok(_) => (),
        Err(e) => {
            let err = Error::io(&base_dir, e);
            recorder.record_session(
                ERROR_PHASE,
                err.to_file_stat(Some(&base_dir)),
                ErrorFlags::default(),
            );
            return Err(err);
        }
    }

    // Stable workload + resume position, mirroring the rehash phase bar: the
    // max is the *overall* set of files the phase will ever move, the position
    // the already-moved count, so a resumed run restarts exactly where it left
    // off (and the early `pending == 0` short-circuit runs on the remainder).
    let total = rt.db.count_files_to_move(true)?;
    let done = rt.db.count_moved_files(true)?;
    let pending = total.saturating_sub(done);
    if pending == 0 {
        rt.db.drop_canonical_move_queue()?;
        return Ok(());
    }
    rt.db.create_canonical_move_queue()?;
    rt.db.populate_canonical_move_queue(true)?;

    // Progress: one per-worker byte bar for the in-flight copy plus a dedicated
    // files-done sub-bar. Neither the phase bar nor the global is touched here —
    // both are owned by `materialize_link_tree` (a moved canonical counts there,
    // exactly once).
    rt.progress.create_thread_bars(BarKind::Bytes, rt.config.process.io_jobs);
    let moved_bar = rt.progress.push_sub_bar("files moved", BarKind::Count);
    moved_bar.set_length(total);
    moved_bar.set_position(done);
    let mut bars = Vec::<ProgressBar>::new();
    for i in 0..rt.config.process.io_jobs {
        bars.push(rt.progress.thread_bar(i));
    }

    let (work_s, work_r) = bounded::<StrippedRecord>(WORK_CAPACITY);
    let (out_s, out_r) = bounded::<Option<CopyOutcome>>(OUT_CAPACITY);
    let mut thread_handles = Vec::with_capacity(rt.config.process.io_jobs);

    let cache_dir = rt.config.paths.extract_cache_dir();
    let keep_stage = rt.config.process.cleanup.keep_stage;
    let no_reflink = rt.config.placement.no_reflink;
    for i in 0..rt.config.process.io_jobs {
        let base = base_dir.clone();
        let cache = cache_dir.clone();
        let bar = bars[i].clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let sh = rt.shutdown.clone();
        let res = thread::Builder::new()
            .name(format!("copy-worker-{i}").into())
            .spawn(move || copy_canonical_worker(
                base, cache, keep_stage, no_reflink, bar, sh, wr, os
            ))
            .expect("spawn copy worker");
        thread_handles.push(res);
    }
    drop(work_r);
    drop(out_s);

    // Feed cursor over `canonical_move_queue`: `queue_index` is the last
    // consumed queue position. The pull re-filters to rows still lacking
    // `AtLinkSource`, so an already-moved file is never re-pulled.
    let mut queue_index = 0u64;
    let pull = || {
        let rows = rt.db.pull_canonical_move_queue::<StrippedRecord>(
            queue_index, FEED_CHUNK as u64)?;
        if !rows.is_empty() {
            queue_index = rows[rows.len() - 1].0;
        }
        Ok(rows.into_iter().map(|(_q, row)| row).collect::<Vec<_>>())
    };
    let apply = |pending: &mut Vec<CopyOutcome>| -> Result<()> {
        let items = take(pending);
        let n = rt.db.ingest_copy_results(&items)?;
        for result in items {
            if let Err((id, Error::FileStat(e))) = result{
                recorder.record_file(id, ERROR_PHASE, e, ErrorFlags::default());
            };
        }
        moved_bar.inc(n);
        Ok(())
    };

    send_receive_loop(
        rt.shutdown,
        work_s,
        out_r,
        thread_handles,
        DRAIN_CHUNK,
        false,
        pull,
        |_| Ok(()),
        || Ok(true),
        apply)?;
    rt.progress.drop_thread_bars();
    recorder.flush()?;
    if rt.shutdown.is_interrupted() {
        return Err(Error::Interrupted);
    }
    rt.db.drop_canonical_move_queue()?;
    Ok(())
}

/// Function iterates through all entries in the out tree which are not dirs and not unknown builds
/// the tree.
/// Files are linked to the link source and all others links, fifo, char dev, block dev are created,
/// sockets noted but cannot be created
pub fn materialize_link_tree(rt: &ExtractRTArgs, recorder: &mut Recorder) -> Result<()> {
    let dir_name = match &rt.config.placement.link_source {
        None => PathBuf::from(".sources"),
        Some(v) => v.to_path_buf(),
    };
    let shutdown = rt.shutdown.clone();
    let base_dir = rt.config.paths.extraction_root().join(&dir_name);
    let results = Mutex::new(Vec::new());
    let pool = ThreadPoolBuilder::new()
        .num_threads(rt.config.process.io_jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    let inner_process = |canonical: &FileRecord, out: &OutTreeRecord| -> Result<()> {
        shutdown.check_between_files()?;
        let result = if matches!(canonical.ftype, FileType::File) {
            let content_id = canonical
                .content_id()
                .expect("PRECONDITION: Moved successfully, content_id must exist")
                .0;
            // Compute the target for link
            let link_target =
                if rt.config.placement.absolute_links || rt.config.placement.use_hard_links {
                    base_dir.join(content_id)
                } else {
                    let up = relative_pardirs_to_dir(
                        rt.config.paths.extraction_root(),
                        &out.abs_path,
                    );
                    up.join(&dir_name).join(content_id)
                };
            if out.abs_path.exists() {
                return Err(Error::Config(format!(
                    "Found existing path {}. Link Tree must be empty.",
                    out.abs_path.display())));
            }

            // Actually build the link
            let base_res = if rt.config.placement.use_hard_links {
                fs::hard_link(link_target, &out.abs_path)
            } else {
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
                Err(e) => Err(FileStatError::Io {
                    path: out.abs_path.to_path_buf(), source: e,
                }),
            }
        } else {
            assert!(!matches!(canonical.ftype, FileType::Unknown), "'unknown' not permitted!");
            assert!(!matches!(canonical.ftype, FileType::Directory), "'Directory' not permitted!");
            build_other(&canonical, &out, rt.config.placement.recreate_none_file_entries)
        };
        results
            .lock()
            .expect("Result lock for link in place poisoned")
            .push((out.id, result.err()));
        rt.progress.inc_both(1);
        Ok(())
    };

    batched_loop(
        // INFO: list_out_tree_for_linking filters ('dir' and 'unknown')
        |bs| rt.db.list_out_tree_for_linking(bs, true),
        BATCH_SIZE,
        |entries: Vec<(FileRecord, OutTreeRecord)>| {
            let parallel = pool.install(|| {
                entries.par_iter().try_for_each(
                    |(can, out)| inner_process(can, out)
                )
            });

            // Check Pool Result
            match parallel {
                Ok(()) => (),
                Err(Error::Interrupted) => (), // Exit
                Err(e) => return Err(e),
            }

            // Get the results
            let new_res = Vec::new();
            let linked = std::mem::replace(
                &mut *results
                .lock()
                .expect("hash results lock"),
                new_res
            );

            // Apply results to db
            rt.db.ingest_results_link_tree(&linked)?;
            for (id, err) in linked {
                match err {
                    None => { let _ = rt.db.set_out_tree_flag(id, OutTreeFlag::Placed, true)?; }
                    Some(e) => {
                        recorder.record_out_tree(id, ERROR_PHASE, e, ErrorFlags::default());
                    }
                }
            }
            Ok(())
        }
    )?;
    recorder.flush()?;
    Ok(())
}

/// Which output entries a [`materialize_loop`] pass rebuilds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MaterializeKind {
    /// File canonical rows (`out_tree.canonical_id = id`): copy from the cache.
    Files,
    /// Rows aliasing an already-placed canonical row: hardlink into place.
    Hardlinks,
    /// Symlinks, fifos, char/block devices (no copy).
    Others,
}

/// Unit of placement work: the file row plus the output row it materializes,
/// and for hardlinks the placed canonical output row that is aliased.
#[derive(Clone)]
struct MaterializeWork {
    canonical: FileRecord,
    out: OutTreeRecord,
    out_canon: Option<OutTreeRecord>,
}

type MaterializeOutcome = std::result::Result<MaterializeResult, (OutTreeId, Error)>;

/// Set the canonical_id of the out_tree.
/// PRECONDITION: This function should ouly be called with link_tree == false
/// Iterate through the out_tree and reflink / copy all files into placed which are marked as
/// (hardlink) canonicals. (out_tree.canonical_id = id)
pub fn materialize_files(rt: &ExtractRTArgs, recorder: &mut Recorder) -> Result<()> {
    materialize_loop(rt, recorder, MaterializeKind::Files)
}

/// Create all the hardlinks after the copy stage.
/// PRECONDITION: Function must be called after the [`materialize_files`]
pub fn materialize_hardlinks(rt: &ExtractRTArgs, recorder: &mut Recorder) -> Result<()> {
    materialize_loop(rt, recorder, MaterializeKind::Hardlinks)
}

/// Final step, pass through all the remaining entries which could be materialized:
/// (symlink, fifo, character device, block device, socket)
pub fn materialize_others(rt: &ExtractRTArgs, recorder: &mut Recorder) -> Result<()> {
    materialize_loop(rt, recorder, MaterializeKind::Others)
}

/// Shared materializer for the three non-link placement passes. `Files` copies
/// canonical payloads from the extract cache (size-DESC order via the ordering
/// queue), `Hardlinks` aliases the placed canonical row, `Others` recreates
/// special files. Owns this pass's worker threads and thread bars.
fn materialize_loop(rt: &ExtractRTArgs, recorder: &mut Recorder, kind: MaterializeKind)
    -> Result<()> {
    let is_hardlink = matches!(kind, MaterializeKind::Hardlinks);
    // Reflink flag-tracking and the size-DESC queue both apply only to the copy
    // pass (`Files`); derive the second from the first so the two can't drift.
    let is_files =  matches!(kind, MaterializeKind::Files);
    let ordered = is_files;
    let set_reflink = is_files;

    if ordered {
        rt.db.create_materialize_queue()?;
        rt.db.populate_materialize_queue()?;
        // Copy work streams byte progress into a per-worker Bytes bar; the single
        // actions are near-instant, so their bars stay hidden.
        rt.progress.create_thread_bars(BarKind::Bytes, rt.config.process.io_jobs);

    }

    let mut bars = Vec::<Option<ProgressBar>>::new();
    for i in 0..rt.config.process.io_jobs {
        bars.push(if ordered { Some(rt.progress.thread_bar(i)) } else { None });
    }

    let (work_s, work_r) = bounded::<MaterializeWork>(WORK_CAPACITY);
    let (out_s, out_r) = bounded::<Option<MaterializeOutcome>>(OUT_CAPACITY);
    let mut thread_handles = Vec::with_capacity(rt.config.process.io_jobs);
    for i in 0..rt.config.process.io_jobs {
        let bar = bars[i].clone();
        let wr = work_r.clone();
        let os = out_s.clone();
        let sh = rt.shutdown.clone();
        let cfg = rt.config.clone();
        let res = thread::Builder::new()
            .name(format!("materialize-worker-{i}").into())
            .spawn(move || materialize_worker(kind, cfg, bar, sh, wr, os))
            .expect("spawn materialize worker");
        thread_handles.push(res);
    }
    drop(work_r);
    drop(out_s);

    // Feed cursor per kind: the ordered queue (positional) for files, a plain
    // `out_tree` id cursor for the two lister-driven kinds (same contract: a
    // handed row is never re-pulled; the loop drains every committed outcome).
    let mut queue_index = 0u64;
    let mut last_id = OutTreeId(0);
    let pull = || -> Result<Vec<MaterializeWork>> {
        match kind {
            MaterializeKind::Files => {
                let rows = rt.db.pull_materialize_queue::<FileRecord>(
                    queue_index, FEED_CHUNK as u64)?;
                if !rows.is_empty() {
                    queue_index = rows[rows.len() - 1].0;
                }
                let out = rows
                    .into_iter()
                    .map(|(_q, canonical, out)| MaterializeWork {
                        canonical, out, out_canon: None,
                    }).collect();
                Ok(out)
            }
            MaterializeKind::Hardlinks => {
                let entries = rt.db.list_out_tree_for_hardlinks::<FileRecord>(
                    &last_id, FEED_CHUNK as u64)?;
                if !entries.is_empty() {
                    last_id = entries[entries.len() - 1].1.id;
                }
                let out = entries
                    .into_iter()
                    .map(|(can, otr_can, otr_ent)|
                        MaterializeWork {
                        canonical: can, out: otr_ent, out_canon: Some(otr_can),
                    }).collect();
                Ok(out)
            }
            MaterializeKind::Others => {
                let entries = rt.db.list_out_tree_others::<FileRecord>(
                    &last_id, FEED_CHUNK as u64)?;
                if !entries.is_empty() {
                    last_id = entries[entries.len() - 1].1.id;
                }
                let out = entries
                    .into_iter()
                    .map(|(canonical, out)| MaterializeWork {
                        canonical, out, out_canon: None,
                    }).collect();
                Ok(out)
            }
        }
    };
    let apply = |pending: &mut Vec<MaterializeOutcome>| -> Result<()> {
        let items = take(pending);
        if items.is_empty() {
            return Ok(());
        }
        let n = items.len() as u64;
        process_results(items, recorder, rt.db, is_hardlink, set_reflink)?;
        rt.progress.inc_both(n);
        Ok(())
    };

    send_receive_loop(
        rt.shutdown,
        work_s,
        out_r,
        thread_handles,
        DRAIN_CHUNK,
        false,
        pull,
        |_| Ok(()),
        || Ok(true),
        apply)?;

    rt.progress.drop_thread_bars();
    recorder.flush()?;
    if rt.shutdown.is_interrupted() {
        return Err(Error::Interrupted);
    }
    if ordered {
        rt.db.drop_materialize_queue()?;
    }
    Ok(())
}

/// Worker thread: rebuilds one output entry at a time, forwards the outcome.
/// Owns its bar; only touches the channels, `tracing` and its `Shutdown` clone.
/// A force abort mid-copy drops the in-flight outcome (`None`) so the row stays
/// pending for the resumed run.
fn materialize_worker(
    kind: MaterializeKind,
    config: ExtractConfig,
    bar: Option<ProgressBar>,
    shutdown: Shutdown,
    work: Receiver<MaterializeWork>,
    out: Sender<Option<MaterializeOutcome>>) -> () {
    let cache_dir = config.paths.extract_cache_dir();
    loop {
        match work.recv() {
            Ok(w) => {
                if shutdown.is_interrupted() {
                    break;
                }
                if let Some(b) = bar.as_ref() {
                    b.reset();
                    b.set_length(w.canonical.size);
                    b.set_message(format!("Materializing {}", w.out.abs_path.display()));
                }
                match materialize_one(&kind, &cache_dir, &config, &w, &shutdown, bar.as_ref()) {
                    Some(outcome) => {
                        out.send(Some(outcome)).expect("materialize worker: result channel closed");
                    }
                    None => break,
                }
            }
            Err(_) => break,
        }
    }
    out.send(None).expect("materialize worker: result channel closed");
}

/// Rebuild a single output entry per its kind. Returns `None` when the run was
/// force-aborted mid-copy: the in-flight row must not be committed.
fn materialize_one(
    kind: &MaterializeKind,
    cache_dir: &Path,
    config: &ExtractConfig,
    w: &MaterializeWork,
    shutdown: &Shutdown,
    bar: Option<&ProgressBar>)
    -> Option<MaterializeOutcome> {
    let stripped = w.canonical.to_stripped();
    match kind {
        MaterializeKind::Files => {
            let target = &w.out;
            let id = w.canonical.content_id()
                .expect("PRECONDITION FAILED: Enqueued files must have a content_id");
            let src = cache_dir.join(id.0);

            match check_path(config, &src, &stripped) {
                Err(e) => Some(Err((target.id, Error::FileStat(e)))),
                Ok((false, conflict, removed)) => Some(Ok(MaterializeResult {
                    id: target.id, placed: false, conflict, removed, used_copy: false,
                })),
                Ok((true, conflict, removed)) => {
                    tracing::info!("Materializing to {}", target.abs_path.display());
                    match copy_single_file(
                        target.id, &src, &target.abs_path, shutdown,
                        config.placement.no_reflink, bar) {

                        Err((_, Error::Interrupted)) => None,
                        Err(other) => Some(Err(other)),
                        Ok((fid, used_copy)) => Some(Ok(MaterializeResult {
                            id: fid, placed: true, conflict, removed, used_copy,
                        })),
                    }
                }
            }
        }
        MaterializeKind::Hardlinks => {
            let out_canon = w.out_canon.as_ref()
                .expect("PRECONDITION FAILED: hardlink work must carry the canonical out row");
            let dst = &w.out.abs_path;
            let idx = w.out.id;

            match check_path(config, dst, &stripped) {
                Err(e) => Some(Err((idx, Error::FileStat(e)))),
                Ok((false, conflict, removed)) => Some(Ok(MaterializeResult {
                    id: idx, placed: false, conflict, removed, used_copy: false,
                })),
                Ok((true, conflict, removed)) => match fs::hard_link(
                    &out_canon.abs_path, dst) {

                    Err(e) => Some(Err((idx, Error::io(dst, e)))),
                    Ok(()) => Some(Ok(MaterializeResult {
                        id: idx, placed: true, conflict, removed, used_copy: false,
                    })),
                },
            }
        }
        MaterializeKind::Others => {
            let target = &w.out;
            let idx = target.id;

            match check_path(config, &target.abs_path, &stripped) {
                Err(e) => Some(Err((idx, Error::FileStat(e)))),
                Ok((false, conflict, removed)) => Some(Ok(MaterializeResult {
                    id: idx, placed: false, conflict, removed, used_copy: false,
                })),
                Ok((true, conflict, removed)) => match build_other(
                    &w.canonical, target, config.placement.recreate_none_file_entries) {

                    Err(e) => Some(Err((idx, Error::FileStat(e)))),
                    Ok(()) => Some(Ok(MaterializeResult {
                        id: idx, placed: true, conflict, removed, used_copy: false,
                    })),
                },
            }
        }
    }
}

/// Function recreates all special files it can. Importantly, files, directories and unknown
/// types are not valid file types for the function and will cause a panic
fn build_other(canonical: &FileRecord, out_tree: &OutTreeRecord, try_special: bool)
    -> std::result::Result<(), FileStatError> {
    match canonical.ftype {
        FileType::File => panic!("PRECONDITION ERROR: build_other does not treat files"),
        FileType::Directory => panic!("PRECONDITION ERROR: build_other does not treat directories"),
        FileType::Unknown => panic!("PRECONDITION ERROR: build_other does not treat unknown"),
        FileType::Socket => {
            tracing::info!("Received Socket at {}, skipping", &out_tree.abs_path.display());
            Ok(())
        }
        #[cfg(unix)]
        FileType::Symlink(_) => match &canonical.link_dst {
            None => Ok(()),
            Some(dst) => match std::os::unix::fs::symlink(dst, &out_tree.abs_path) {
                Ok(_) => Ok(()),
                Err(e) => Err(FileStatError::Io {
                    path: out_tree.abs_path.to_path_buf(),
                    source: e,
                }),
            },
        },
        #[cfg(windows)]
        FileType::Symlink(LinkType::Directory) => match &canonical.link_dst {
            None => Ok(()),
            Some(dst) => match std::os::windows::fs::symlink_dir(dst, &out_tree.abs_path) {
                Ok(_) => Ok(()),
                Err(e) => Err(FileStatError::Io{path: out_tree.abs_path.to_path_buf(), source: e}),
            },
        },
        #[cfg(windows)]
        FileType::Symlink(_) => match &canonical.link_dst {
            None => Ok(()),
            Some(dst) => match std::os::windows::fs::symlink_file(dst, &out_tree.abs_path) {
                Ok(_) => Ok(()),
                Err(e) => Err(FileStatError::Io{path: out_tree.abs_path.to_path_buf(), source: e}),
            },
        },
        FileType::FIFO => {
            if !try_special {
                return Ok(());
            }
            match nix::unistd::mkfifo(&out_tree.abs_path, Mode::from_bits_truncate(0o644)) {
                Ok(_) => Ok(()),
                Err(e) => Err(FileStatError::Nix {
                    path: out_tree.abs_path.to_path_buf(),
                    source: e,
                }),
            }
        }
        f @ (FileType::BlockDevice | FileType::CharacterDevice) => {
            let (dev_type, s_flag) = match f {
                FileType::BlockDevice => ("block device", SFlag::S_IFBLK),
                FileType::CharacterDevice => ("character device", SFlag::S_IFCHR),
                _ => panic!(
                    "Unexpected file type {}, expected BlockDevice or CharacterDevice",
                    f.as_str()
                ),
            };
            if !try_special {
                return Ok(());
            }
            if canonical.major.is_none() || canonical.minor.is_none() {
                tracing::error!(
                    "Could not create {dev_type} at {}, major and/or minor is missing",
                    out_tree.abs_path.display()
                );
                return Err(FileStatError::general(
                    Some(&out_tree.abs_path),
                    "Missing major and/or minor".to_string()
                ));
            }
            let dev = makedev(
                canonical.major.unwrap() as u32,
                canonical.minor.unwrap() as u32,
            );
            let create_res = mknod(
                &out_tree.abs_path,
                s_flag,
                Mode::from_bits_truncate(0o644),
                dev,
            );
            match create_res {
                Ok(_) => Ok(()),
                Err(e) => Err(FileStatError::Nix {
                    path: out_tree.abs_path.to_path_buf(),
                    source: e,
                }),
            }
        }
    }
}

/// Get the number of files, hardlinks and others. Also produce
pub fn status_message_rebuilding(rt: &ExtractRTArgs)
    -> Result<(u64, u64, u64, u64, u64, u64)> {
    let all_canonicals = rt.db.count_out_tree_canonicals(None)?;
    let all_hardlinks = rt.db.count_out_tree_hardlinks(None)?;
    let all_other = rt.db.count_out_tree_others(None)?;
    let materialized_canonicals = rt.db.count_out_tree_canonicals(Some(false))?;
    let materialized_hardlinks = rt.db.count_out_tree_hardlinks(Some(false))?;
    let materialized_other = rt.db.count_out_tree_others(Some(false))?;

    let other_msg = if rt.config.placement.recreate_none_file_entries {
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
        materialized_other))
}

fn process_results(
    results: Vec<std::result::Result<MaterializeResult, (OutTreeId, Error)>>,
    recorder: &mut Recorder,
    db: &Database,
    is_hardlink: bool,
    set_reflink: bool)
    -> Result<()> {

    db.ingest_materialize_results(&results, is_hardlink, set_reflink)?;
    for result in results {
        match result {
            Err((id, Error::FileStat(e))) => {
                recorder.record_out_tree(id, ERROR_PHASE, e, ErrorFlags::default())
            }
            _ => ()
        }
    }
    Ok(())
}

// -------------------------------------------------------------------------------------------------
// Util
// -------------------------------------------------------------------------------------------------

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
    let depth = below.components().count();
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

/// Worker thread: pulls one canonical row at a time, copies it into the link
/// source, removes the cache source on success (when not keeping the stage),
/// forwards the outcome. Owns its bar. On a force abort the in-flight outcome
/// is dropped (`None`) so the row stays pending for the resumed run.
fn copy_canonical_worker(
    base_dir: PathBuf,
    cache_dir: PathBuf,
    keep_stage: bool,
    no_reflink: bool,
    bar: ProgressBar,
    shutdown: Shutdown,
    work: Receiver<StrippedRecord>,
    out: Sender<Option<CopyOutcome>>) -> () {
    loop {
        match work.recv() {
            Ok(row) => {
                if shutdown.check_between_files().is_err() {
                    break;
                }
                bar.reset();
                bar.set_length(row.size);
                bar.set_message(format!("Moving {}", row.abs_path.display()));
                match copy_one_to_source(
                    &base_dir, &cache_dir, keep_stage, no_reflink, &row, &shutdown, Some(&bar)) {
                    Some(outcome) => {
                        out.send(Some(outcome)).expect("copy worker: result channel closed");
                    }
                    None => break,
                }
            }
            Err(_) => break,
        }
    }
    out.send(None).expect("copy worker: result channel closed");
}

/// Move a single canonical payload into the link-source dir. Returns `None`
/// when the run was force-aborted mid-copy: the in-flight row must not be
/// committed.
fn copy_one_to_source(
    base_dir: &Path,
    cache_dir: &Path,
    keep_stage: bool,
    no_reflink: bool,
    row: &StrippedRecord,
    shutdown: &Shutdown,
    pb: Option<&ProgressBar>)
    -> Option<std::result::Result<(FileId, bool), (FileId, Error)>> {

    let cid = row.content_id().expect("Content id existed, when extracting.");
    let src = cache_dir.join(&cid.0);
    let dst = base_dir.join(&cid.0);
    match copy_single_file(row.id, &src, &dst, shutdown, no_reflink, pb) {
        Ok((fid, is_copy)) => {
            // The `.sources` copy is what the link tree links FROM and must
            // stay; the cache source is redundant once it landed. Dropping it
            // frees the stage dir ahead of the end-of-place cleanup sweep.
            if !keep_stage {
                let _ = fs::remove_file(&src);
            }
            Some(Ok((fid, is_copy)))
        }
        Err((_, Error::Interrupted)) => None,
        Err(other) => Some(Err(other)),
    }
}

/// Copy a single file from a to b. Function implements a shutdown check to avoid long blocking
/// Error contains the Error as well as the file id to link against,
/// Ok contains the id as well as bool which is false if reflink was used and true if copy was used.
/// `pb` (when present) is advanced by copied byte deltas, so the caller's bar
/// shows live progress on big files.
/// INFO: Function returns Variants Interrupted and FileStatError
fn copy_single_file<ID>(fid: ID, src: &Path, dst: &Path, shutdown: &Shutdown, no_reflink: bool,
    pb: Option<&ProgressBar>)
    -> std::result::Result<(ID, bool), (ID, Error)> {
    // Attempt to reflink
    if !no_reflink {
        let worked = reflink::reflink(src, dst);
        if worked.is_ok() {
            return Ok((fid, false));
        }
    }

    // Failed, perform sparse copy
    let mut prev = 0u64;
    let spc_res = sparse_cp::sparse_copy_with_progress(
        src, dst, 4096,
        |bytes: u64, _size: u64, _dur: std::time::Duration| -> Result<()> {
            if let Some(pb) = pb {
                pb.inc(bytes - prev);
            }
            prev = bytes;
            shutdown.check_in_flight()
        }
    );

    // Handle result; sparse-cp converts io errors via `From<io::Error>` with an
    // empty path, so re-attach the destination on the way out.
    match spc_res {
        Ok(_) => Ok((fid, true)),
        Err(e) => {
            let _ = fs::remove_file(dst);
            Err((fid, annotate_copy_error(e, dst)))
        }
    }
}

/// Attach `dst` to a copy error. `Interrupted` is preserved verbatim (the phase
/// loop branches on it); other `FileStat` error kinds pass through untouched.
fn annotate_copy_error(e: Error, dst: &Path) -> Error {
    match e {
        Error::FileStat(FileStatError::Io { source, .. }) => Error::FileStat(FileStatError::Io {
            path: dst.to_path_buf(),
            source,
        }),
        other => other,
    }
}

/// Build a given directory path for later extraction.
/// PRECONDITION: Calling function must ensure no_create_dir is false
pub fn build_path(
    config: &ExtractConfig,
    already_checked: &mut PathBuf,
    target: &Path,
    out_tree_id: OutTreeId,
    recorder: &mut Recorder)
    -> Result<()> {
    debug_assert!(!config.placement.no_create_dir,
                  "INVARIANT ERROR: build_path may not be called with no_create_dir");
    let mut prefix = PathBuf::new();
    let mut start = 0u64;
    let mut capture_error = |path: &PathBuf, iof: fn(&Path) -> io::Result<()>| match iof(&path) {
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
            break;
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
            continue;
        }

        // Some but no dir
        if prefix.exists() && !prefix.is_dir() {
            if config.placement.remove_and_replace {
                // INFO: Raise; if fs fails here, almost guaranteed that extraction fails.
                capture_error(&prefix, |p: &Path| fs::remove_file(p))?;
                capture_error(&prefix, |p: &Path| fs::create_dir_all(p))?;
                tracing::info!("Replaced non-dir with dir at path: {printable}");
                continue;
            } else {
                return Err(Error::Config(format!(
                    "Encountered existing non-directory path at extraction \
                             location where directory was needed: {printable}"
                )));
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
/// Perform the pre-materialize check.
/// First perform the conflict policy check based on mtime, then check the file type, lastly, if
/// required remove previous file.
/// On error, the file on the file system wins.
/// Result:
/// - first bool is true iff the file can be written to the path
/// - second bool is true iff there was a conflict which resolved in favor of extracting
/// - third bool is true iff there was a file was removed during the prep process.
fn check_path(config: &ExtractConfig, tgt: &Path, rec: &StrippedRecord)
    -> std::result::Result<(bool, bool, bool), FileStatError> {
    if !tgt.exists() {
        return Ok((true, false, false));
    };

    // PRECONDITION: Path exists
    let file_metadata = match tgt.symlink_metadata() {
        Err(e) => {
            return if config.placement.silent_conflicts {
                tracing::info!(
                    "Leaving {}, could not assess conflict due to inaccessible file metadata",
                    tgt.display()
                );
                Ok((false, true, false))
            } else {
                Err(FileStatError::Io {
                    path: tgt.to_path_buf(),
                    source: e,
                })
            };
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
                        tgt.display()
                    );
                }
                return Ok((false, true, false));
            }
            Some(t) => t,
        };
        let (res_mtime, _, _) = files::get_file_times(&file_metadata);
        let file_mtime = match res_mtime {
            Err(e) => {
                return if config.placement.silent_conflicts {
                    tracing::info!("Leaving {}, could not retrieve file mtime", tgt.display());
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
                            path: Some(tgt.to_path_buf()),
                            message: "File is too old.".to_string(),
                        })
                    } else {
                        tracing::info!("Could not extract to {}, file is too old.", tgt.display());
                        Ok((false, true, false))
                    };
                }
            }
            ConflictPolicy::PreferOlder => {
                if db_mtime >= file_mtime {
                    return if !config.placement.silent_conflicts {
                        Err(FileStatError::General {
                            path: Some(tgt.to_path_buf()),
                            message: "File is new old.".to_string(),
                        })
                    } else {
                        tracing::info!("Could not extract to {}, file is too new.", tgt.display());
                        Ok((false, true, false))
                    };
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
                    message: format!(
                        "File type mismatch, expected {} got {}",
                        rec.ftype.as_str(),
                        ftype.as_str()
                    ),
                })
            } else {
                tracing::info!("File type mismatch, expected {} got {}",
                    rec.ftype.as_str(), ftype.as_str());
                Ok((false, true, false))
            };
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
                    Ok((false, true, true))
                } else {
                    Err(FileStatError::Io { path: tgt.to_path_buf(), source: e })
                }
            }
        }
    } else {
        Ok((true, true, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error as IoError, ErrorKind};

    #[test]
    fn annotate_copy_error_attaches_dst_to_io() {
        let dst = PathBuf::from("/out/dst.txt");
        let err = Error::FileStat(FileStatError::Io {
            path: PathBuf::new(),
            source: IoError::new(ErrorKind::NotFound, "boom"),
        });
        match annotate_copy_error(err, &dst) {
            Error::FileStat(FileStatError::Io { path, source }) => {
                assert_eq!(path, dst);
                assert_eq!(source.kind(), ErrorKind::NotFound);
            }
            other => panic!("expected FileStat(Io), got {other:?}"),
        }
    }

    #[test]
    fn annotate_copy_error_preserves_interrupted() {
        let dst = PathBuf::from("/out/dst.txt");
        assert!(matches!(
            annotate_copy_error(Error::Interrupted, &dst),
            Error::Interrupted
        ));
    }

    #[test]
    fn annotate_copy_error_passes_unknown_errors_through() {
        let dst = PathBuf::from("/out/dst.txt");
        let general = Error::FileStat(FileStatError::General {
            path: Some(PathBuf::from("/somewhere")),
            message: "nope".into(),
        });
        assert!(matches!(
            annotate_copy_error(general, &dst),
            Error::FileStat(FileStatError::General { .. })
        ));
        assert!(matches!(
            annotate_copy_error(Error::Other(anyhow::anyhow!("x")), &dst),
            Error::Other(_)
        ));
    }
}
