//! Place: copy/link cached payloads to final output paths.

use crate::cli::ConflictPolicy;
use crate::common::files;
use crate::common::{batched_loop, batched_stepped_loop, send_receive_loop};
use crate::config::{ExtractConfig, ExtractPipelinePhase};
use crate::db::Database;
use crate::db::flags::ErrorFlags;
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

const ERROR_PHASE: ErrorPhase = ErrorPhase::Extract(ExtractPipelinePhase::Place);

pub fn run(rt: &ExtractRTArgs) -> Result<()> {
    debug_assert!(rt.db.placement_prologue_done()?,
                  "PRECONDITION FAILED: PlacementPrologue must complete before place");
    let mut recorder = Recorder::new(rt.db, !rt.config.process.no_errors, Some(rt.config.process.auto_flush_limit));
    let corpus = rt.db.count_out_tree_rows()?;
    rt.progress.set_phase_total(corpus);
    let (ac, pc, ah, ph, ao, po) = status_message_rebuilding(rt)?;
    tracing::info!(
        "starting place: total {}, processing {}, pending {}",
        corpus,
        ac + ah + ao,
        pc + ph + po,
    );

    // Step 1.1 empty out source root prior to starting extraction.
    if rt.config.placement.clean_target && rt.config.placement.one_top_level.is_some() {
        assert!(!rt.config.placement.no_create_dir,
                "INVARIANT ERROR: clean_target => no_create_dir is false");
        let root = rt.config.effective_extraction_root();
        capture_error(&mut recorder, &root, |p| fs::remove_dir_all(p))?;
        capture_error(&mut recorder, &root, |p| fs::create_dir_all(p))?;
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
        materialize_files(rt, &mut recorder)?;
        cleanup_stage(&rt, &mut recorder)?; // Free space!

        materialize_hardlinks(rt, &mut recorder)?;
        materialize_others(rt, &mut recorder)?;
    }
    let (placed, _ref_linked, _conflict, _removed, _errored, _skipped) =
        rt.db.apply_flags_to_files()?;
    tracing::info!(
        "place done: processed {} of {}, {} error(s)",
        placed,
        corpus,
        rt.db.count_errors(Some("place"))?,
    );

    cleanup_stage(&rt, &mut recorder)?;

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
fn prepare_extraction_dir(
    rt: &ExtractRTArgs,
    recorder: &mut Recorder) -> Result<()> {
    debug_assert!(rt.config.effective_extraction_root().is_absolute(),
                  "INVARIANT ERROR: extraction root is not absolute");
    debug_assert!(rt.db.out_tree_is_built().expect("out_tree meta"),
                  "PRECONDITION FAILED: OutTree must be built to run this function");
    debug_assert!(!rt.db.dir_tree_is_built().expect("dir_tree meta"),
                  "PRECONDITION FAILED: Only run if the dir tree is not built yet");
    tracing::warn!("Building the Directory tree cannot be gracefully interrupted!");

    let mut already_checked = rt.config.effective_extraction_root().to_path_buf();

    batched_stepped_loop(
        rt.config.process.batch_size,
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
fn copy_canonicals_to_source(rt: &ExtractRTArgs, recorder: &mut Recorder)
    -> Result<()> {
    let dir_name = match &rt.config.placement.link_source {
        None => PathBuf::from(".sources"),
        Some(v) => v.to_path_buf(),
    };

    let base_dir = rt.config.effective_extraction_root().join(dir_name);
    capture_error(recorder, &base_dir, |p| fs::create_dir_all(p))?;

    let total = rt.db.count_files_to_move(true)?;
    let done = rt.db.count_moved_files(true)?;
    let pending = total.saturating_sub(done);

    if pending == 0 {
        rt.db.drop_canonical_move_queue()?;
        return Ok(());
    }
    rt.db.create_canonical_move_queue()?;
    rt.db.populate_canonical_move_queue(true)?;

    let moved_bar = rt.progress.push_sub_bar("files moved", BarKind::Count);
    moved_bar.set_length(total);
    moved_bar.set_position(done);

    rt.progress.create_thread_bars(BarKind::Bytes, rt.config.process.io_jobs);
    let mut bars = Vec::<ProgressBar>::new();
    for i in 0..rt.config.process.io_jobs {
        bars.push(rt.progress.thread_bar(i));
    }

    let (work_s, work_r) = bounded::<StrippedRecord>(rt.config.process.work_capacity);
    let (out_s, out_r) = bounded::<Option<CopyOutcome>>(rt.config.process.out_capacity);
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

    // queue feeder
    let mut queue_index = 0u64;
    let pull = || {
        let rows = rt.db.pull_canonical_move_queue::<StrippedRecord>(
            queue_index, rt.config.process.feed_chunk as u64)?;
        if !rows.is_empty() {
            queue_index = rows[rows.len() - 1].0;
        }
        Ok(rows.into_iter().map(|(_q, row)| row).collect::<Vec<_>>())
    };

    // queue drainer
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
        rt.config.process.drain_chunk,
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

    if let Err(e) = capture_error(
        recorder, &rt.config.paths.extract_cache_dir(), |p| fs::remove_dir_all(p)) {
        tracing::warn!("Failed to delete extract cache at: {} with error {e}",
            rt.config.paths.extract_cache_dir().display()
        );
    }

    rt.db.drop_canonical_move_queue()?;
    Ok(())
}

/// Function iterates through all entries in the out tree which are not dirs and not unknown builds
/// the tree.
/// Files are linked to the link source and all others links, fifo, char dev, block dev are created,
/// sockets noted but cannot be created
fn materialize_link_tree(rt: &ExtractRTArgs, recorder: &mut Recorder) -> Result<()> {
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
            base_res.map_err(|e| FileStatError::io(&out.abs_path, e))
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
        rt.config.process.batch_size,
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
                if let Some(e) = err {
                    recorder.record_out_tree(id, ERROR_PHASE, e, ErrorFlags::default());
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
fn materialize_files(rt: &ExtractRTArgs, recorder: &mut Recorder) -> Result<()> {
    materialize_loop(rt, recorder, MaterializeKind::Files)
}

/// Create all the hardlinks after the copy stage.
/// PRECONDITION: Function must be called after the [`materialize_files`]
fn materialize_hardlinks(rt: &ExtractRTArgs, recorder: &mut Recorder) -> Result<()> {
    materialize_loop(rt, recorder, MaterializeKind::Hardlinks)
}

/// Final step, pass through all the remaining entries which could be materialized:
/// (symlink, fifo, character device, block device, socket)
fn materialize_others(rt: &ExtractRTArgs, recorder: &mut Recorder) -> Result<()> {
    materialize_loop(rt, recorder, MaterializeKind::Others)
}

/// Shared materializer for the three non-link placement passes. `Files` copies
/// canonical payloads from the extract cache (size-DESC order via the ordering
/// queue), `Hardlinks` aliases the placed canonical row, `Others` recreates
/// special files. Owns this pass's worker threads and thread bars.
fn materialize_loop(rt: &ExtractRTArgs, recorder: &mut Recorder, kind: MaterializeKind)
    -> Result<()> {
    let ordered = matches!(kind, MaterializeKind::Files);;

    // Progress bars, only needed for file copy.
    let mut bars = Vec::<Option<ProgressBar>>::new();
    if matches!(kind, MaterializeKind::Files) {
        rt.db.create_materialize_queue()?;
        rt.db.populate_materialize_queue()?;
        // Copy work streams byte progress into a per-worker Bytes bar; the single
        // actions are near-instant, so their bars stay hidden.
        rt.progress.create_thread_bars(BarKind::Bytes, rt.config.process.io_jobs);
        for i in 0..rt.config.process.io_jobs {
            bars.push(Some(rt.progress.thread_bar(i)));
        }
    } else {
        bars.resize(rt.config.process.io_jobs, None);
    }

    let (work_s, work_r) = bounded::<MaterializeWork>(rt.config.process.work_capacity);
    let (out_s, out_r) = bounded::<Option<MaterializeOutcome>>(rt.config.process.out_capacity);
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

    // Trackers of the last element processed in the ordered queue.
    let mut queue_index = 0u64;
    let mut last_id = OutTreeId(0);
    // INFO: Cannot create three different closures. Would cause type conflict!
    let pull = || -> Result<Vec<MaterializeWork>> {
        match kind {
            MaterializeKind::Files => {
                let rows = rt.db.pull_materialize_queue::<FileRecord>(
                    queue_index, rt.config.process.feed_chunk as u64)?;
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
                    &last_id, rt.config.process.feed_chunk as u64)?;
                if !entries.is_empty() {
                    last_id = entries[entries.len() - 1].2.id; // need .2 rather than .1!
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
                    &last_id, rt.config.process.feed_chunk as u64)?;
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
        process_results(
            items,
            recorder,
            rt.db,
            matches!(kind, MaterializeKind::Hardlinks),
            matches!(kind, MaterializeKind::Files),
        )?;
        rt.progress.inc_both(n);
        Ok(())
    };

    send_receive_loop(
        rt.shutdown,
        work_s,
        out_r,
        thread_handles,
        rt.config.process.drain_chunk,
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
                        out.send(Some(outcome))
                            .expect("materialize worker: result channel closed");
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
    let outcome = match kind {

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
                    tracing::debug!("Materializing to {} ({})", target.abs_path.display(),
                        stripped.ftype.as_str());
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
    };
    match outcome {
        Some(Ok(ref r)) if r.placed => {
            tracing::debug!("placed {} ({})", w.out.abs_path.display(),
                            stripped.ftype.as_str());
        }
        Some(Err((_idx, ref e @ Error::FileStat(_)))) => {
            tracing::error!("place failed {} ({}): {}", w.out.abs_path.display(),
                            stripped.ftype.as_str(), e);
        }
        _ => (),
    }
    outcome
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
            tracing::debug!("Received Socket at {}, skipping ({})",
                &out_tree.abs_path.display(), canonical.ftype.as_str());
            Ok(())
        }
        #[cfg(unix)]
        FileType::Symlink(_) => match &canonical.link_dst {
            None => Ok(()),
            Some(dst) => std::os::unix::fs::symlink(dst, &out_tree.abs_path)
                .map_err(|e| FileStatError::io(&out_tree.abs_path, e))
        },
        #[cfg(windows)]
        FileType::Symlink(LinkType::Directory) => match &canonical.link_dst {
            None => Ok(()),
            Some(dst) => std::os::windows::fs::symlink_dir(dst, &out_tree.abs_path)
                .map_err(|e| FileStatError::io(&out_tree.abs_path, e))
        },
        #[cfg(windows)]
        FileType::Symlink(_) => match &canonical.link_dst {
            None => Ok(()),
            Some(dst) => std::os::windows::fs::symlink_file(dst, &out_tree.abs_path)
                .map_err(|e| FileStatError::io(&out_tree.abs_path, e))
        },
        FileType::FIFO => {
            if !try_special {
                return Ok(());
            }
            nix::unistd::mkfifo(&out_tree.abs_path, Mode::from_bits_truncate(0o644))
                .map_err(|e| FileStatError::nix(&out_tree.abs_path, e))
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
            mknod(&out_tree.abs_path, s_flag, Mode::from_bits_truncate(0o644), dev)
                .map_err(|e| FileStatError::nix(&out_tree.abs_path, e))
        }
    }
}

/// Get the number of files, hardlinks and others. This is tuple notation
/// 1. Total number of canonical files
/// 2. Number of canonical files still pending
/// 3. Total number of hard links
/// 4. Number of hardlinks still pending
/// 5. Total number of other entries
/// 6. Number of other entries still pending
fn status_message_rebuilding(rt: &ExtractRTArgs)
    -> Result<(u64, u64, u64, u64, u64, u64)> {
    let all_canonicals = rt.db.count_out_tree_canonicals(None)?;
    let all_hardlinks = rt.db.count_out_tree_hardlinks(None)?;
    let all_other = rt.db.count_out_tree_others(None)?;
    // `Some(false)` selects rows neither placed nor errored — the pending work.
    let pending_canonicals = rt.db.count_out_tree_canonicals(Some(false))?;
    let pending_hardlinks = rt.db.count_out_tree_hardlinks(Some(false))?;
    let pending_other = rt.db.count_out_tree_others(Some(false))?;

    let other_msg = if rt.config.placement.recreate_none_file_entries {
        &format!("{} of other entries, {pending_other} remaining.",
                 all_other - pending_other)
    } else {
        ""
    };

    tracing::info!("
        Placement Phase:
        {} of files already copied, {pending_canonicals} remaining.
        {} of hardlinks already created, {pending_hardlinks} remaining.
        {other_msg}",
        all_canonicals - pending_canonicals,
        all_hardlinks - pending_hardlinks,
    );
    Ok((all_canonicals,
        pending_canonicals,
        all_hardlinks,
        pending_hardlinks,
        all_other,
        pending_other))
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
    // INFO no check of components required. Normal is implied by the preconditions.
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
fn copy_single_file<ID>(
    fid: ID,
    src: &Path,
    dst: &Path,
    shutdown: &Shutdown,
    no_reflink: bool,
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
            Err((fid, annotate_path(e, dst)))
        }
    }
}

/// Attach `dst` to a copy error. `Interrupted` is preserved verbatim (the phase
/// loop branches on it); other `FileStat` error kinds pass through untouched.
fn annotate_path(e: Error, dst: &Path) -> Error {
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
fn build_path(
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
    let mut capture_error = |path: &PathBuf, iof: fn(&Path) -> io::Result<()>|
        match iof(&path) {

            Ok(_) => Ok(()),
            Err(e) => {
                recorder.record_out_tree(
                    out_tree_id,
                    ERROR_PHASE,
                    FileStatError::copy_io(&path, &e),
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
            tracing::debug!("Replaced symlink with dir at path: {printable}");
            continue;
        }

        // Some but no dir
        if prefix.exists() && !prefix.is_dir() {
            if config.placement.remove_and_replace {
                // INFO: Raise; if fs fails here, almost guaranteed that extraction fails.
                capture_error(&prefix, |p: &Path| fs::remove_file(p))?;
                capture_error(&prefix, |p: &Path| fs::create_dir_all(p))?;
                tracing::debug!("Replaced non-dir with dir at path: {printable}");
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
            tracing::debug!("Replaced non-dir with dir at path: {printable}");
            continue;
        }

        // By exclusion principle -> dir or symlink with keep_dir_symlink
        assert!(prefix.exists()
                && (prefix.is_dir()
                || (prefix.is_symlink() && config.placement.keep_dir_symlink)),
                "INVARIANT ERROR: Directory should be created or error raised.")
    }

    assert_eq!(target, prefix, "target and prefix must be identical");
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
                tracing::warn!(
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
        // INFO: why no atime?
        //  - unstable i.e. most likely changed by our reads
        //  - no real fallback on unix atime <-> mtime
        //  - complicated design choice what and what not to support with mtime.
        // mtime of record
        let db_mtime = match rec.mtime {
            None => {
                if !config.placement.silent_conflicts {
                    tracing::warn!(
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
                    tracing::warn!("Leaving {}, could not retrieve file mtime", tgt.display());
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
                        tracing::warn!("Could not extract to {}, file is too old.", tgt.display());
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
                        tracing::warn!("Could not extract to {}, file is too new.", tgt.display());
                        Ok((false, true, false))
                    };
                }
            }
        }
    }

    // Check if the file types are a match (cannot write do the file operations on symlink).
    let ftype = match files::determine_file_type(&file_metadata, tgt) {
        Ok(ft) => ft,
        Err((ft, failed_path, e)) => {
            // INFO: Error means ftype unknown. Can proceed, add error regardless
            if config.placement.silent_conflicts {
                tracing::warn!("Error while determining file type of {}. Got {}",
                tgt.display(), &e);
            } else {
                return Err(FileStatError::Io{ path: failed_path, source: e })
            };
            ft
        }
    };
    let remove = if ftype != rec.ftype {
        if config.placement.remove_and_replace {
            tracing::warn!("File type mismatch, expected {} got {}",
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
                tracing::warn!("File type mismatch, expected {} got {}",
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
                    tracing::warn!("Failed to remove preexisting path {} with error {}",
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

/// Capture the error of io functions (usually fs::...). The error is captured and passed out.
fn capture_error(rec: &mut Recorder, path: &PathBuf, iof: fn(&Path) -> io::Result<()>)
    -> Result<()> {
    match iof(&path) {
        Ok(_) => Ok(()),
        Err(e) => {
            rec.record_session(
                ERROR_PHASE,
                FileStatError::copy_io(&path, &e),
                ErrorFlags::default(),
            );
            Err(Error::io(&path, e))
        }
    }
}

/// Attempt to clean up the stage if it is not requested to be kept and handle any errors
/// accordingly.
fn cleanup_stage(rt: &ExtractRTArgs, recorder: &mut Recorder) -> Result<()> {
    if !rt.config.process.cleanup.keep_stage {
        let cache_dir = rt.config.paths.extract_cache_dir();
        match capture_error(
            recorder,
            &cache_dir.to_path_buf(), |p| fs::remove_dir_all(p)) {

            Ok(()) => Ok(()),
            Err(e) => {
                if rt.config.process.fail_fast {
                    Err(Error::Config(format!(
                        "Failed to clean up stage directors '{}' with error {}",
                        cache_dir.display(), e)))
                } else {
                    tracing::warn!("Failed to clean up stage directors '{}' with error {}",
                        cache_dir.display(), e);
                    Ok(())
                }
            }
        }
    } else {
        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::files::original_extension;
    use crate::db::content_id::content_id_from_digest;
    use crate::db::flags::{FileFlag, FileFlags, OutTreeFlag, OutTreeFlags};
    use crate::db::types::{LinkType, NewFileRecord, NewOutTreeRow};
    use crate::test_support::{ExtractWorld, seed_filter_reasons};
    use chrono::{DateTime, Utc};
    use rusqlite::named_params;
    use std::io::{Error as IoError, ErrorKind};
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[test]
    fn annotate_copy_error_attaches_dst_to_io() {
        let dst = PathBuf::from("/out/dst.txt");
        let err = Error::FileStat(FileStatError::Io {
            path: PathBuf::new(),
            source: IoError::new(ErrorKind::NotFound, "boom"),
        });
        match annotate_path(err, &dst) {
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
            annotate_path(Error::Interrupted, &dst),
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
            annotate_path(general, &dst),
            Error::FileStat(FileStatError::General { .. })
        ));
        assert!(matches!(
            annotate_path(Error::Other(anyhow::anyhow!("x")), &dst),
            Error::Other(_)
        ));
    }

    // ---------------------------------------------------------------------
    // PlaceWorld: local seeder impl on the shared `ExtractWorld`.
    // ---------------------------------------------------------------------

    impl ExtractWorld {
        /// Real-disk place world: `for_place_test` config re-rooted under the
        /// tempdir (`out/` = extraction root, `estage/` = extract cache), a
        /// single worker for deterministic ordered results.
        fn place_test(tune: impl FnOnce(&mut ExtractConfig)) -> Self {
            Self::with_config(|dp| {
                let mut config = ExtractConfig::for_place_test(dp.join("out"), false, 4);
                config.paths.work_dir = dp.join("estage");
                config.process.io_jobs = 1;
                config.process.jobs = 1;
                tune(&mut config);
                config
            })
        }

        fn root(&self) -> PathBuf {
            self.config.paths.extraction_root().to_path_buf()
        }

        /// Create + return the extraction root.
        fn ensure_root(&self) -> PathBuf {
            fs::create_dir_all(self.root()).expect("create root");
            self.root()
        }

        /// Insert a canonical `files` row: self-linked, `rehashed`,
        /// `FileExtracted`, joint filter passed, digest `[sha;20]`.
        fn add_place_file(&self, abs: &str, size: u64, sha: u8, ftype: FileType) -> FileId {
            self.add_place_file_inode(abs, size, sha, ftype, None, None)
        }

        /// `add_file` plus a captured `(dev, inode)` (real hardlink groups).
        fn add_place_file_inode(
            &self, abs: &str, size: u64, sha: u8, ftype: FileType,
            dev: Option<u64>, inode: Option<u64>) -> FileId {
            self.seed_filter();
            let path = PathBuf::from(abs);
            self.db.insert_file(&NewFileRecord {
                abs_path: path.clone(),
                abs_path_raw: None,
                ext: original_extension(&path),
                size,
                mtime: None, atime: None, ctime: None,
                uid: None, gid: None, mode: None,
                ftype: Some(ftype),
                xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None,
                link_dst: None, link_dst_raw: None,
                device_id: dev, inode_id: inode, major: None, minor: None,
            }).expect("insert file");
            let id = self.db.file_id_by_abs_path(&path).expect("lookup").expect("row");
            seed_filter_reasons(&self.db, id);
            self.db.mark_self_canonical(id).expect("self canonical");
            self.db.update_file_inspection_per_id(id, [sha; 20], 0, false).expect("digest");
            self.db.set_file_flag(id, FileFlag::FileExtracted, true).expect("extracted");
            self.db.mark_file_phase(id, FilePhase::Rehashed).expect("phase");
            id
        }

        /// A non-canonical `files` row pointing at `canonical` as its content
        /// canonical. Dups carry no payload and no `FileExtracted` (extract
        /// never unpacks their content — only the dedup canonical has a member).
        fn add_dup_file(&self, abs: &str, canonical: FileId) -> FileId {
            self.seed_filter();
            let path = PathBuf::from(abs);
            self.db.insert_file(&NewFileRecord {
                abs_path: path.clone(),
                abs_path_raw: None,
                ext: original_extension(&path),
                size: 0,
                mtime: None, atime: None, ctime: None,
                uid: None, gid: None, mode: None,
                ftype: Some(FileType::File),
                xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None,
                link_dst: None, link_dst_raw: None,
                device_id: None, inode_id: None, major: None, minor: None,
            }).expect("insert dup");
            let id = self.db.file_id_by_abs_path(&path).expect("lookup").expect("row");
            seed_filter_reasons(&self.db, id);
            self.db.set_canonical(id, canonical).expect("set canonical");
            self.db.mark_file_phase(id, FilePhase::Rehashed).expect("phase");
            id
        }

        /// A self-canonical symlink `files` row with a link target.
        fn add_place_symlink(&self, abs: &str, link_dst: &str) -> FileId {
            self.seed_filter();
            let path = PathBuf::from(abs);
            self.db.insert_file(&NewFileRecord {
                abs_path: path.clone(),
                abs_path_raw: None,
                ext: original_extension(&path),
                size: 0,
                mtime: None, atime: None, ctime: None,
                uid: None, gid: None, mode: None,
                ftype: Some(FileType::Symlink(LinkType::File)),
                xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None,
                link_dst: Some(PathBuf::from(link_dst)), link_dst_raw: None,
                device_id: None, inode_id: None, major: None, minor: None,
            }).expect("insert symlink");
            let id = self.db.file_id_by_abs_path(&path).expect("lookup").expect("row");
            seed_filter_reasons(&self.db, id);
            self.db.mark_self_canonical(id).expect("self canonical");
            self.db.update_file_inspection_per_id(id, [2; 20], 0, false).expect("digest");
            self.db.mark_file_phase(id, FilePhase::Rehashed).expect("phase");
            id
        }

        /// Insert an `out_tree` row and set its canonical_id.
        fn add_out(&self, abs: &str, file_id: FileId, canonical: Option<OutTreeId>) -> OutTreeId {
            let mut flags = OutTreeFlags::default();
            flags.set(OutTreeFlag::IsDirectory, false);
            let rows = vec![NewOutTreeRow {
                abs_path: PathBuf::from(abs),
                file_id: Some(file_id),
                flags,
            }];
            let id = self.db.insert_out_tree_rows(&rows).expect("insert out")[0];
            if let Some(c) = canonical {
                self.db.with_transaction(|conn| {
                    conn.execute(
                        "UPDATE out_tree SET canonical_id = :c WHERE id = :id",
                        named_params! { ":c": c.0, ":id": id.0 },
                    ).expect("set canonical");
                    Ok(())
                }).expect("canonical tx");
            }
            id
        }

        /// A pure directory `out_tree` row (no `file_id`).
        fn add_out_dir(&self, abs: &str) -> OutTreeId {
            let mut flags = OutTreeFlags::default();
            flags.set(OutTreeFlag::IsDirectory, true);
            let rows = vec![NewOutTreeRow {
                abs_path: PathBuf::from(abs),
                file_id: None,
                flags,
            }];
            self.db.insert_out_tree_rows(&rows).expect("insert out dir")[0]
        }

        /// Write the cache payload for a canonical file row; returns the path.
        /// The content id derives from the row's own `abs_path` extension, so the
        /// file lands under the name `materialize_one` will look up.
        fn put_payload(&self, id: FileId, bytes: &[u8]) -> PathBuf {
            let rec = self.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row");
            let cid = rec.content_id().expect("canonical file has content id");
            let p = self.config.paths.extract_cache_dir().join(&cid.0);
            fs::create_dir_all(p.parent().expect("parent")).expect("cache dir");
            fs::write(&p, bytes).expect("write payload");
            p
        }

        fn out_flag(&self, id: OutTreeId, flag: OutTreeFlag) -> bool {
            self.db.get_out_tree_flag(id, flag).expect("get flag")
        }

        fn file_flag(&self, id: FileId, flag: FileFlag) -> bool {
            self.db.get_file_flag(id, flag).expect("get flag")
        }

        /// Set a file row's stored mtime (RFC3339, as the pipeline writes it).
        fn set_file_mtime(&self, id: FileId, t: DateTime<Utc>) {
            self.db.with_transaction(|conn| {
                conn.execute(
                    "UPDATE files SET mtime = :t WHERE id = :id",
                    named_params! { ":t": t.to_rfc3339(), ":id": id.0 },
                ).expect("set mtime");
                Ok(())
            }).expect("mtime tx");
        }
    }

    /// Pure `StrippedRecord` builder (no DB).
    fn mk_stripped(id: i64, abs: &str, ftype: FileType, mtime: Option<DateTime<Utc>>)
        -> StrippedRecord {
        StrippedRecord {
            id: FileId(id),
            abs_path: PathBuf::from(abs),
            abs_path_raw: None,
            ext: original_extension(Path::new(abs)),
            size: 0,
            sha1: None,
            mtime, atime: None, ctime: None,
            ftype,
            device_id: None, inode_id: None,
            canonical_id: Some(FileId(id)),
            flags: FileFlags::default(),
            phase: FilePhase::Rehashed,
            new_name: None,
        }
    }

    /// Pure `FileRecord` builder (canonical: `canonical_id = id`, hashed).
    #[allow(clippy::too_many_arguments)]
    fn mk_file(id: i64, abs: &str, size: u64, sha: u8, ftype: FileType,
               link_dst: Option<&str>, major: Option<u64>, minor: Option<u64>) -> FileRecord {
        FileRecord {
            id: FileId(id),
            abs_path: PathBuf::from(abs),
            ext: original_extension(Path::new(abs)),
            size,
            sha1: Some([sha; 20]),
            mtime: None, atime: None, ctime: None,
            uid: None, gid: None, username: None, groupname: None, mode: None,
            ftype,
            device_id: None, inode_id: None,
            major, minor,
            xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None,
            link_dst: link_dst.map(PathBuf::from),
            include_reason_archive: None, exclude_reason_archive: None,
            include_reason_extract: None, exclude_reason_extract: None,
            canonical_id: Some(FileId(id)),
            flags: FileFlags::default(),
            phase: FilePhase::Rehashed,
            new_name: None,
        }
    }

    fn mk_out(id: i64, abs: &str) -> OutTreeRecord {
        OutTreeRecord {
            id: OutTreeId(id),
            abs_path: PathBuf::from(abs),
            file_id: Some(FileId(id)),
            flags: OutTreeFlags::default(),
            canonical_id: Some(OutTreeId(id)),
        }
    }

    fn mk_work(canonical: FileRecord, out: OutTreeRecord, out_canon: Option<OutTreeRecord>)
        -> MaterializeWork {
        MaterializeWork { canonical, out, out_canon }
    }

    // ---------------------------------------------------------------------
    // build_path
    // ---------------------------------------------------------------------

    #[test]
    fn build_path_creates_missing_dirs() {
        let world = ExtractWorld::place_test(|c| c.placement.no_create_dir = false);
        let root = world.ensure_root();
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        let target = root.join("a/b/c");
        let mut already = root.clone();
        build_path(&world.config, &mut already, &target, OutTreeId(1), &mut recorder).expect("build");
        assert!(target.is_dir());
        assert_eq!(already, target);
        assert_eq!(recorder.buffered(), 0);
    }

    #[test]
    fn build_path_replaces_symlink_segment() {
        let world = ExtractWorld::place_test(|c| c.placement.no_create_dir = false);
        let root = world.ensure_root();
        let outside = world.path("outside");
        fs::create_dir_all(&outside).expect("outside");
        std::os::unix::fs::symlink(&outside, root.join("a")).expect("symlink");
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        let target = root.join("a/b");
        let mut already = root.clone();
        build_path(&world.config, &mut already, &target, OutTreeId(1), &mut recorder).expect("build");
        assert!(root.join("a").is_dir() && !root.join("a").is_symlink(), "symlink replaced");
        assert!(target.is_dir());
    }

    #[test]
    fn build_path_keeps_dir_symlink() {
        let world = ExtractWorld::place_test(|c| {
            c.placement.no_create_dir = false;
            c.placement.keep_dir_symlink = true;
        });
        let root = world.ensure_root();
        let real = world.path("real");
        fs::create_dir_all(&real).expect("real");
        std::os::unix::fs::symlink(&real, root.join("a")).expect("symlink");
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        let target = root.join("a/b");
        let mut already = root.clone();
        build_path(&world.config, &mut already, &target, OutTreeId(1), &mut recorder).expect("build");
        assert!(root.join("a").is_symlink(), "symlink kept");
        assert!(real.join("b").is_dir(), "subdir created under the link target");
    }

    #[test]
    fn build_path_replaces_non_dir_with_dir() {
        let world = ExtractWorld::place_test(|c| {
            c.placement.no_create_dir = false;
            c.placement.remove_and_replace = true;
        });
        let root = world.ensure_root();
        fs::write(root.join("a"), b"file").expect("write");
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        let target = root.join("a/b");
        let mut already = root.clone();
        build_path(&world.config, &mut already, &target, OutTreeId(1), &mut recorder).expect("build");
        assert!(root.join("a").is_dir(), "non-dir replaced");
        assert!(target.is_dir());
    }

    #[test]
    fn build_path_non_dir_without_replace_errors() {
        let world = ExtractWorld::place_test(|c| c.placement.no_create_dir = false);
        let root = world.ensure_root();
        fs::write(root.join("a"), b"file").expect("write");
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        let target = root.join("a/b");
        let mut already = root.clone();
        let res = build_path(&world.config, &mut already, &target, OutTreeId(1), &mut recorder);
        assert!(matches!(res, Err(Error::Config(_))));
    }

    #[test]
    fn build_path_panics_on_sibling_root() {
        let world = ExtractWorld::place_test(|c| c.placement.no_create_dir = false);
        let root = world.ensure_root();
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        // Sibling of the extraction root: the common prefix is shallower than the root.
        let sibling = root.parent().expect("parent").join("other");
        let mut already = root.clone();
        let res = catch_unwind(AssertUnwindSafe(|| {
            build_path(&world.config, &mut already, &sibling.join("x"), OutTreeId(1), &mut recorder)
        }));
        assert!(res.is_err(), "target outside extract dir must panic");
    }

    #[cfg(unix)]
    #[test]
    fn build_path_records_error_when_create_fails() {
        use std::os::unix::fs::PermissionsExt;
        let world = ExtractWorld::place_test(|c| c.placement.no_create_dir = false);
        let root = world.ensure_root();
        let blocked = root.join("blocked");
        fs::create_dir_all(&blocked).expect("blocked");
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o500)).expect("chmod");
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        let target = blocked.join("sub");
        let mut already = root.clone();
        let res = build_path(&world.config, &mut already, &target, OutTreeId(1), &mut recorder);
        assert!(matches!(res, Err(Error::FileStat(FileStatError::Io { .. }))));
        assert_eq!(recorder.buffered(), 1);
    }

    // ---------------------------------------------------------------------
    // check_path
    // ---------------------------------------------------------------------

    #[test]
    fn check_path_missing_target_is_placeable() {
        let world = ExtractWorld::place_test(|_| {});
        let tgt = world.root().join("nope.bin");
        let rec = mk_stripped(1, "/x.bin", FileType::File, None);
        assert_eq!(check_path(&world.config, &tgt, &rec).expect("check"),
                   (true, false, false));
    }

    #[test]
    fn check_path_missing_db_mtime_keeps_existing() {
        for silent in [false, true] {
            let world = ExtractWorld::place_test(|c| {
                c.placement.conflict_policy = ConflictPolicy::PreferNewer;
                c.placement.silent_conflicts = silent;
            });
            let root = world.ensure_root();
            let tgt = root.join("x.bin");
            fs::write(&tgt, b"x").expect("write");
            let rec = mk_stripped(1, "/x.bin", FileType::File, None);
            assert_eq!(check_path(&world.config, &tgt, &rec).expect("check"),
                       (false, true, false));
        }
    }

    fn live_dt(tgt: &Path) -> DateTime<Utc> {
        DateTime::<Utc>::from(fs::metadata(tgt).expect("meta").modified().expect("mtime"))
    }

    #[test]
    fn check_path_prefer_newer_resolves_by_mtime() {
        let world = ExtractWorld::place_test(|c| c.placement.conflict_policy = ConflictPolicy::PreferNewer);
        let root = world.ensure_root();
        let tgt = root.join("x.bin");
        fs::write(&tgt, b"x").expect("write");
        let live = live_dt(&tgt);
        let hour = chrono::TimeDelta::seconds(3600);
        // db > live -> proceed
        let rec_old = mk_stripped(1, "/x.bin", FileType::File, Some(live + hour));
        assert_eq!(check_path(&world.config, &tgt, &rec_old).expect("check"),
                   (true, true, false));
        // db == live -> keep (tie favours the on-disk copy)
        let rec_eq = mk_stripped(1, "/x.bin", FileType::File, Some(live));
        assert!(check_path(&world.config, &tgt, &rec_eq).is_err());
        // db < live -> keep
        let rec_new = mk_stripped(1, "/x.bin", FileType::File, Some(live - hour));
        assert!(check_path(&world.config, &tgt, &rec_new).is_err());
    }

    #[test]
    fn check_path_prefer_newer_silent_keeps_existing() {
        let world = ExtractWorld::place_test(|c| {
            c.placement.conflict_policy = ConflictPolicy::PreferNewer;
            c.placement.silent_conflicts = true;
        });
        let root = world.ensure_root();
        let tgt = root.join("x.bin");
        fs::write(&tgt, b"x").expect("write");
        let rec = mk_stripped(1, "/x.bin", FileType::File, Some(live_dt(&tgt)));
        assert_eq!(check_path(&world.config, &tgt, &rec).expect("check"),
                   (false, true, false));
    }

    #[test]
    fn check_path_prefer_older_resolves_by_mtime() {
        let world = ExtractWorld::place_test(|c| c.placement.conflict_policy = ConflictPolicy::PreferOlder);
        let root = world.ensure_root();
        let tgt = root.join("x.bin");
        fs::write(&tgt, b"x").expect("write");
        let live = live_dt(&tgt);
        let hour = chrono::TimeDelta::seconds(3600);
        // db < live -> proceed
        let rec_new = mk_stripped(1, "/x.bin", FileType::File, Some(live - hour));
        assert_eq!(check_path(&world.config, &tgt, &rec_new).expect("check"),
                   (true, true, false));
        // db == live -> keep
        let rec_eq = mk_stripped(1, "/x.bin", FileType::File, Some(live));
        assert!(check_path(&world.config, &tgt, &rec_eq).is_err());
        // db > live -> keep
        let rec_old = mk_stripped(1, "/x.bin", FileType::File, Some(live + hour));
        assert!(check_path(&world.config, &tgt, &rec_old).is_err());
    }

    #[test]
    fn check_path_replace_ignores_mtime() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let tgt = root.join("x.bin");
        fs::write(&tgt, b"x").expect("write");
        let live = live_dt(&tgt);
        let hour = chrono::TimeDelta::seconds(3600);
        for dt in [live - hour, live, live + hour] {
            let rec = mk_stripped(1, "/x.bin", FileType::File, Some(dt));
            assert_eq!(check_path(&world.config, &tgt, &rec).expect("check"),
                       (true, true, false));
        }
    }

    #[test]
    fn check_path_type_mismatch_replace_removes() {
        let world = ExtractWorld::place_test(|c| c.placement.remove_and_replace = true);
        let root = world.ensure_root();
        let real = root.join("real.bin");
        fs::write(&real, b"r").expect("write");
        let tgt = root.join("x.bin");
        std::os::unix::fs::symlink(&real, &tgt).expect("symlink");
        let rec = mk_stripped(1, "/x.bin", FileType::File, None);
        assert_eq!(check_path(&world.config, &tgt, &rec).expect("check"),
                   (true, true, true));
        assert!(!tgt.exists(), "mismatched symlink removed");
    }

    #[test]
    fn check_path_type_mismatch_loud_errors() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let real = root.join("real.bin");
        fs::write(&real, b"r").expect("write");
        let tgt = root.join("x.bin");
        std::os::unix::fs::symlink(&real, &tgt).expect("symlink");
        let rec = mk_stripped(1, "/x.bin", FileType::File, None);
        assert!(matches!(
            check_path(&world.config, &tgt, &rec),
            Err(FileStatError::General { .. })
        ));
    }

    #[test]
    fn check_path_type_mismatch_silent_keeps_existing() {
        let world = ExtractWorld::place_test(|c| c.placement.silent_conflicts = true);
        let root = world.ensure_root();
        let real = root.join("real.bin");
        fs::write(&real, b"r").expect("write");
        let tgt = root.join("x.bin");
        std::os::unix::fs::symlink(&real, &tgt).expect("symlink");
        let rec = mk_stripped(1, "/x.bin", FileType::File, None);
        assert_eq!(check_path(&world.config, &tgt, &rec).expect("check"),
                   (false, true, false));
    }

    #[test]
    fn check_path_unlink_first_removes_file() {
        let world = ExtractWorld::place_test(|c| c.placement.unlink_first = true);
        let root = world.ensure_root();
        let tgt = root.join("x.bin");
        fs::write(&tgt, b"x").expect("write");
        let rec = mk_stripped(1, "/x.bin", FileType::File, None);
        assert_eq!(check_path(&world.config, &tgt, &rec).expect("check"),
                   (true, true, true));
        assert!(!tgt.exists());
    }

    #[test]
    fn check_path_unlink_first_nonempty_dir_fails() {
        for silent in [false, true] {
            let world = ExtractWorld::place_test(|c| {
                c.placement.unlink_first = true;
                c.placement.silent_conflicts = silent;
            });
            let root = world.ensure_root();
            let tgt = root.join("d");
            fs::create_dir(&tgt).expect("dir");
            fs::write(tgt.join("inner"), b"x").expect("write");
            let rec = mk_stripped(1, "/d", FileType::Directory, None);
            let res = check_path(&world.config, &tgt, &rec);
            match (silent, res) {
                (true, Ok((false, true, true))) => (),
                (false, Err(FileStatError::Io { .. })) => (),
                (silent, other) => panic!("silent={silent} got {other:?}"),
            }
        }
    }

    // ---------------------------------------------------------------------
    // copy_single_file
    // ---------------------------------------------------------------------

    #[test]
    fn copy_single_file_plain_copy() {
        let world = ExtractWorld::place_test(|_| {});
        let src = world.path("src.bin");
        let dst = world.path("dst.bin");
        fs::write(&src, b"payload").expect("write");
        let res = copy_single_file(FileId(1), &src, &dst, &Shutdown::detached(), true, None);
        assert_eq!(res.expect("copy"), (FileId(1), true));
        assert_eq!(fs::read(&dst).expect("read"), b"payload");
    }

    #[test]
    fn copy_single_file_reflink_or_copy_content_equal() {
        let world = ExtractWorld::place_test(|_| {});
        let src = world.path("src.bin");
        let dst = world.path("dst.bin");
        fs::write(&src, b"reflink-me").expect("write");
        let (id, used_copy) = copy_single_file(FileId(1), &src, &dst, &Shutdown::detached(), false, None)
            .expect("copy");
        assert_eq!(id, FileId(1));
        assert_eq!(fs::read(&dst).expect("read"), b"reflink-me");
        // reflink may fall back to a copy on unsupported filesystems; only the
        // content and the (id, bool) shape are guaranteed.
        let _ = used_copy;
    }

    #[test]
    fn copy_single_file_force_aborts() {
        let world = ExtractWorld::place_test(|_| {});
        let src = world.path("src.bin");
        let dst = world.path("dst.bin");
        fs::write(&src, vec![0xAB; 1024 * 1024]).expect("write");
        let sh = Shutdown::detached();
        sh.request_force();
        let res = copy_single_file(FileId(1), &src, &dst, &sh, true, None);
        assert!(matches!(res, Err((FileId(1), Error::Interrupted))));
        assert!(!dst.exists(), "aborted copy must not leave a partial dst");
    }

    #[test]
    fn copy_single_file_missing_src_errors_and_cleans_dst() {
        let world = ExtractWorld::place_test(|_| {});
        let dst = world.path("dst.bin");
        fs::write(&dst, b"pre-existing").expect("write");
        let res = copy_single_file(FileId(1), &world.path("nope.bin"), &dst, &Shutdown::detached(), true, None);
        assert!(matches!(res, Err((FileId(1), Error::FileStat(_)))));
        assert!(!dst.exists(), "failed copy must not leave a partial dst");
    }

    // ---------------------------------------------------------------------
    // copy_one_to_source
    // ---------------------------------------------------------------------

    #[test]
    fn copy_one_to_source_copies_and_removes_cache() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let base = root.join(".sources");
        fs::create_dir_all(&base).expect("base");
        let id = world.add_place_file("/a.bin", 3, 1, FileType::File);
        let cid = world.put_payload(id, b"abc");
        let row = world.db.get_file_by_id::<StrippedRecord>(id).expect("get").expect("row");
        let cache = world.config.paths.extract_cache_dir();
        let res = copy_one_to_source(&base, &cache, false, true, &row, &Shutdown::detached(), None);
        let (fid, is_copy) = res.expect("copy").expect("ok");
        assert_eq!(fid, id);
        assert!(is_copy);
        let dst = base.join(cid.file_name().expect("name"));
        assert_eq!(fs::read(&dst).expect("read"), b"abc");
        assert!(!cid.exists(), "cache source removed when not keeping stage");
    }

    #[test]
    fn copy_one_to_source_keeps_cache_when_keeping_stage() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let base = root.join(".sources");
        fs::create_dir_all(&base).expect("base");
        let id = world.add_place_file("/a.bin", 3, 1, FileType::File);
        let cid = world.put_payload(id, b"abc");
        let row = world.db.get_file_by_id::<StrippedRecord>(id).expect("get").expect("row");
        let cache = world.config.paths.extract_cache_dir();
        let res = copy_one_to_source(&base, &cache, true, true, &row, &Shutdown::detached(), None);
        assert!(res.expect("copy").is_ok());
        assert!(cid.exists(), "cache kept when keeping stage");
    }

    #[test]
    fn copy_one_to_source_force_returns_none() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let base = root.join(".sources");
        fs::create_dir_all(&base).expect("base");
        let id = world.add_place_file("/a.bin", 1024 * 1024, 1, FileType::File);
        world.put_payload(id, &vec![0xCD; 1024 * 1024]);
        let row = world.db.get_file_by_id::<StrippedRecord>(id).expect("get").expect("row");
        let cache = world.config.paths.extract_cache_dir();
        let sh = Shutdown::detached();
        sh.request_force();
        let res = copy_one_to_source(&base, &cache, false, true, &row, &sh, None);
        assert!(res.is_none(), "force abort mid-copy drops the in-flight outcome");
    }

    #[test]
    fn copy_one_to_source_missing_src_errors() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let base = root.join(".sources");
        fs::create_dir_all(&base).expect("base");
        let id = world.add_place_file("/a.bin", 3, 1, FileType::File);
        let row = world.db.get_file_by_id::<StrippedRecord>(id).expect("get").expect("row");
        let cache = world.config.paths.extract_cache_dir();
        let res = copy_one_to_source(&base, &cache, false, true, &row, &Shutdown::detached(), None);
        assert!(matches!(res, Some(Err((_, Error::FileStat(_))))));
    }

    // ---------------------------------------------------------------------
    // build_other
    // ---------------------------------------------------------------------

    #[test]
    fn build_other_panics_on_files_dirs_unknown() {
        for ftype in [FileType::File, FileType::Directory, FileType::Unknown] {
            let rec = mk_file(1, "/x", 0, 1, ftype, None, None, None);
            let out = mk_out(1, "/ed/x");
            let res = catch_unwind(AssertUnwindSafe(|| build_other(&rec, &out, true)));
            assert!(res.is_err(), "{ftype:?} must panic");
        }
    }

    #[test]
    fn build_other_skips_socket() {
        let rec = mk_file(1, "/x", 0, 1, FileType::Socket, None, None, None);
        let out = mk_out(1, "/ed/x");
        build_other(&rec, &out, true).expect("ok");
        assert!(!out.abs_path.exists());
    }

    #[test]
    fn build_other_creates_symlink() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let out = mk_out(1, &root.join("link").to_string_lossy());
        let rec = mk_file(1, "/x", 0, 1, FileType::Symlink(LinkType::File), Some("/target"), None, None);
        build_other(&rec, &out, true).expect("ok");
        assert_eq!(fs::read_link(&out.abs_path).expect("readlink"), PathBuf::from("/target"));
    }

    #[test]
    fn build_other_symlink_without_target_is_noop() {
        let rec = mk_file(1, "/x", 0, 1, FileType::Symlink(LinkType::File), None, None, None);
        let out = mk_out(1, "/ed/x");
        build_other(&rec, &out, true).expect("ok");
        assert!(!out.abs_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn build_other_fifo() {
        use std::os::unix::fs::FileTypeExt;
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let out = mk_out(1, &root.join("fifo").to_string_lossy());
        let rec = mk_file(1, "/x", 0, 1, FileType::FIFO, None, None, None);
        build_other(&rec, &out, true).expect("ok");
        assert!(fs::symlink_metadata(&out.abs_path).expect("meta").file_type().is_fifo());
    }

    #[test]
    fn build_other_fifo_disabled_is_noop() {
        let rec = mk_file(1, "/x", 0, 1, FileType::FIFO, None, None, None);
        let out = mk_out(1, "/ed/fifo");
        build_other(&rec, &out, false).expect("ok");
        assert!(!out.abs_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn build_other_char_device() {
        use std::os::unix::fs::FileTypeExt;
        // mknod requires CAP_MKNOD (root); as an unprivileged user the call
        // fails with EPERM — assert the error shape instead of the device.
        if !nix::unistd::geteuid().is_root() {
            let world = ExtractWorld::place_test(|_| {});
            let root = world.ensure_root();
            let out = mk_out(1, &root.join("chardev").to_string_lossy());
            let rec = mk_file(1, "/x", 0, 1, FileType::CharacterDevice, None, Some(1), Some(3));
            assert!(build_other(&rec, &out, true).is_err(), "mknod must fail unprivileged");
            return;
        }
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let out = mk_out(1, &root.join("chardev").to_string_lossy());
        let rec = mk_file(1, "/x", 0, 1, FileType::CharacterDevice, None, Some(1), Some(3));
        build_other(&rec, &out, true).expect("ok");
        assert!(fs::symlink_metadata(&out.abs_path).expect("meta").file_type().is_char_device());
    }

    #[test]
    fn build_other_device_missing_major_or_minor_errors() {
        for (major, minor) in [(None, None), (Some(1), None), (None, Some(3))] {
            let rec = mk_file(1, "/x", 0, 1, FileType::CharacterDevice, None, major, minor);
            let out = mk_out(1, "/ed/x");
            assert!(build_other(&rec, &out, true).is_err(),
                    "major/minor {major:?}/{minor:?} must error");
        }
    }

    #[test]
    fn build_other_device_disabled_is_noop() {
        let rec = mk_file(1, "/x", 0, 1, FileType::BlockDevice, None, Some(1), Some(3));
        let out = mk_out(1, "/ed/x");
        build_other(&rec, &out, false).expect("ok");
        assert!(!out.abs_path.exists());
    }

    // ---------------------------------------------------------------------
    // relative_pardirs_to_dir
    // ---------------------------------------------------------------------

    #[test]
    fn relative_pardirs_to_dir_depth() {
        assert_eq!(
            relative_pardirs_to_dir(Path::new("/ed"), Path::new("/ed/sub/dir/f.txt")),
            PathBuf::from("../../"),
        );
        assert_eq!(
            relative_pardirs_to_dir(Path::new("/ed"), Path::new("/ed/f.txt")),
            PathBuf::from("."),
        );
    }

    #[test]
    fn relative_pardirs_to_dir_panics_outside_root() {
        for (dir, file) in [
            ("/ed", "/other/x.txt"),
            ("/ed", "/edx.txt"),
            ("/ed", "/ed/../escape.txt"),
        ] {
            let res = catch_unwind(AssertUnwindSafe(|| {
                relative_pardirs_to_dir(Path::new(dir), Path::new(file))
            }));
            assert!(res.is_err(), "{file} under {dir} must panic");
        }
    }

    // ---------------------------------------------------------------------
    // materialize_one
    // ---------------------------------------------------------------------

    #[test]
    fn materialize_one_file_places_from_cache() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let cache = world.config.paths.extract_cache_dir();
        let id = world.add_place_file("/src/a.bin", 3, 1, FileType::File);
        world.put_payload(id, b"abc");
        let can = world.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row");
        let out = mk_out(1, &root.join("a.bin").to_string_lossy());
        let w = mk_work(can, out.clone(), None);
        let res = materialize_one(&MaterializeKind::Files, &cache, &world.config, &w,
                                  &Shutdown::detached(), None);
        let r = res.expect("some").expect("ok");
        assert!(r.placed && r.id == out.id);
        assert_eq!(fs::read(&out.abs_path).expect("read"), b"abc");
    }

    #[test]
    fn materialize_one_file_conflict_keeps_existing() {
        let world = ExtractWorld::place_test(|c| {
            c.placement.conflict_policy = ConflictPolicy::PreferNewer;
            c.placement.silent_conflicts = true;
        });
        let root = world.ensure_root();
        let dst = root.join("a.bin");
        fs::write(&dst, b"keep-me").expect("write");
        let live = live_dt(&dst);
        let id = world.add_place_file("/src/a.bin", 3, 1, FileType::File);
        world.set_file_mtime(id, live - chrono::TimeDelta::seconds(3600));
        world.put_payload(id, b"abc");
        let can = world.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row");
        let out = mk_out(1, &dst.to_string_lossy());
        let w = mk_work(can, out.clone(), None);
        let cache = world.config.paths.extract_cache_dir();
        let res = materialize_one(&MaterializeKind::Files, &cache, &world.config, &w,
                                  &Shutdown::detached(), None);
        let r = res.expect("some").expect("ok");
        assert!(!r.placed && r.conflict);
        assert_eq!(fs::read(&dst).expect("read"), b"keep-me", "existing file untouched");
    }

    #[test]
    fn materialize_one_file_copy_error_records() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let cache = world.config.paths.extract_cache_dir();
        let id = world.add_place_file("/src/a.bin", 3, 1, FileType::File);
        let can = world.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row");
        let out = mk_out(1, &root.join("a.bin").to_string_lossy());
        let w = mk_work(can, out, None);
        let res = materialize_one(&MaterializeKind::Files, &cache, &world.config, &w,
                                  &Shutdown::detached(), None);
        assert!(matches!(res, Some(Err((_, Error::FileStat(_))))));
    }

    #[test]
    fn materialize_one_file_force_drops_outcome() {
        // `no_reflink` forces the sparse-copy path where `check_in_flight` fires;
        // a reflink short-circuits before the shutdown check.
        let world = ExtractWorld::place_test(|c| c.placement.no_reflink = true);
        let root = world.ensure_root();
        let cache = world.config.paths.extract_cache_dir();
        let id = world.add_place_file("/src/a.bin", 1024 * 1024, 1, FileType::File);
        world.put_payload(id, &vec![0xAB; 1024 * 1024]);
        let can = world.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row");
        let out = mk_out(1, &root.join("a.bin").to_string_lossy());
        let w = mk_work(can, out, None);
        let sh = Shutdown::detached();
        sh.request_force();
        let res = materialize_one(&MaterializeKind::Files, &cache, &world.config, &w, &sh, None);
        assert!(res.is_none(), "force mid-copy must drop the in-flight outcome");
    }

    #[test]
    fn materialize_one_hardlink_links_to_canonical_out() {
        use std::os::unix::fs::MetadataExt;
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        fs::create_dir_all(root.join("sub")).expect("sub");
        let canon_out = mk_out(1, &root.join("sub/canon.bin").to_string_lossy());
        fs::write(&canon_out.abs_path, b"payload").expect("write");
        let dup = mk_file(2, "/src/dup.bin", 7, 1, FileType::File, None, None, None);
        let dup_out = mk_out(2, &root.join("sub/dup.bin").to_string_lossy());
        let w = mk_work(dup, dup_out.clone(), Some(canon_out));
        let cache = world.config.paths.extract_cache_dir();
        let res = materialize_one(&MaterializeKind::Hardlinks, &cache, &world.config, &w,
                                  &Shutdown::detached(), None);
        let r = res.expect("some").expect("ok");
        assert!(r.placed && r.id == dup_out.id);
        let canon = fs::metadata(root.join("sub/canon.bin")).expect("meta");
        assert_eq!(fs::metadata(&dup_out.abs_path).expect("meta").ino(), canon.ino(),
                   "hardlink shares the canonical inode");
    }

    #[test]
    fn materialize_one_hardlink_missing_canonical_errors() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let canon_out = mk_out(1, &root.join("canon.bin").to_string_lossy());
        let dup = mk_file(2, "/src/dup.bin", 7, 1, FileType::File, None, None, None);
        let dup_out = mk_out(2, &root.join("dup.bin").to_string_lossy());
        let w = mk_work(dup, dup_out.clone(), Some(canon_out));
        let cache = world.config.paths.extract_cache_dir();
        let res = materialize_one(&MaterializeKind::Hardlinks, &cache, &world.config, &w,
                                  &Shutdown::detached(), None);
        assert!(matches!(res, Some(Err((_, Error::FileStat(_))))));
    }

    #[test]
    fn materialize_one_hardlink_conflict_keeps_existing() {
        let world = ExtractWorld::place_test(|c| {
            c.placement.conflict_policy = ConflictPolicy::PreferNewer;
            c.placement.silent_conflicts = true;
        });
        let root = world.ensure_root();
        fs::create_dir_all(root.join("sub")).expect("sub");
        let canon_out = mk_out(1, &root.join("sub/canon.bin").to_string_lossy());
        fs::write(&canon_out.abs_path, b"payload").expect("write");
        let dst = root.join("sub/dup.bin");
        fs::write(&dst, b"keep-me").expect("write");
        let live = live_dt(&dst);
        let mut dup = mk_file(2, "/src/dup.bin", 7, 1, FileType::File, None, None, None);
        dup.mtime = Some(live - chrono::TimeDelta::seconds(3600));
        let dup_out = mk_out(2, &dst.to_string_lossy());
        let w = mk_work(dup, dup_out.clone(), Some(canon_out));
        let cache = world.config.paths.extract_cache_dir();
        let res = materialize_one(&MaterializeKind::Hardlinks, &cache, &world.config, &w,
                                  &Shutdown::detached(), None);
        let r = res.expect("some").expect("ok");
        assert!(!r.placed && r.conflict);
        assert_eq!(fs::read(&dst).expect("read"), b"keep-me");
    }

    #[test]
    fn materialize_one_other_symlink() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let can = mk_file(1, "/src/link", 0, 1, FileType::Symlink(LinkType::File), Some("/tgt"), None, None);
        let out = mk_out(1, &root.join("link").to_string_lossy());
        let w = mk_work(can, out.clone(), None);
        let cache = world.config.paths.extract_cache_dir();
        let res = materialize_one(&MaterializeKind::Others, &cache, &world.config, &w,
                                  &Shutdown::detached(), None);
        let r = res.expect("some").expect("ok");
        assert!(r.placed);
        assert_eq!(fs::read_link(&out.abs_path).expect("readlink"), PathBuf::from("/tgt"));
    }

    // ---------------------------------------------------------------------
    // process_results
    // ---------------------------------------------------------------------

    #[test]
    fn process_results_sets_flags_and_records_errors() {
        let world = ExtractWorld::place_test(|_| {});
        let id = world.add_place_file("/src/a.bin", 3, 1, FileType::File);
        let out_path = world.root().join("a.bin");
        let out = world.add_out(&out_path.to_string_lossy(), id, Some(OutTreeId(1)));
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        let ok = MaterializeResult { id: out, placed: true, conflict: false, removed: false, used_copy: true };
        let err = FileStatError::io(&out_path, IoError::new(ErrorKind::Other, "boom"));
        let results = vec![
            Ok(ok),
            Err((OutTreeId(99), Error::FileStat(err))),
        ];
        process_results(results, &mut recorder, &world.db, false, true).expect("process");
        assert!(world.out_flag(out, OutTreeFlag::Placed));
        assert!(!world.out_flag(out, OutTreeFlag::UsedRefLink), "used_copy=true => no reflink");
        assert_eq!(recorder.buffered(), 1);
    }

    #[test]
    fn process_results_ignores_interrupted() {
        let world = ExtractWorld::place_test(|_| {});
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        let results = vec![Err((OutTreeId(1), Error::Interrupted))];
        process_results(results, &mut recorder, &world.db, false, false).expect("process");
        assert_eq!(recorder.buffered(), 0);
    }

    #[test]
    fn process_results_panics_on_unexpected_error() {
        let world = ExtractWorld::place_test(|_| {});
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        let results = vec![Err((OutTreeId(1), Error::Other(anyhow::anyhow!("x"))))];
        let res = catch_unwind(AssertUnwindSafe(|| {
            process_results(results, &mut recorder, &world.db, false, false)
        }));
        assert!(res.is_err());
    }

    // ---------------------------------------------------------------------
    // workers (channel)
    // ---------------------------------------------------------------------

    #[test]
    fn copy_worker_sends_none_on_dropped_queue() {
        let (s, r) = bounded::<StrippedRecord>(4);
        let (os, or) = bounded::<Option<CopyOutcome>>(4);
        drop(s);
        copy_canonical_worker(
            PathBuf::from("/base"), PathBuf::from("/cache"), false, true,
            indicatif::ProgressBar::hidden(), Shutdown::detached(), r, os,
        );
        assert!(matches!(or.recv(), Ok(None)));
    }

    #[test]
    fn copy_worker_copies_then_sends_none() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let base = root.join(".sources");
        fs::create_dir_all(&base).expect("base");
        let id = world.add_place_file("/src/a.bin", 3, 1, FileType::File);
        world.put_payload(id, b"abc");
        let row = world.db.get_file_by_id::<StrippedRecord>(id).expect("get").expect("row");
        let (s, r) = bounded::<StrippedRecord>(4);
        let (os, or) = bounded::<Option<CopyOutcome>>(4);
        let sh = Shutdown::detached();
        s.send(row.clone()).expect("send");
        drop(s);
        copy_canonical_worker(
            base.clone(), world.config.paths.extract_cache_dir(), false, true,
            indicatif::ProgressBar::hidden(), sh, r, os,
        );
        match or.recv().expect("recv") {
            Some(Ok((fid, _))) => assert_eq!(fid, id),
            other => panic!("expected Ok outcome, got {other:?}"),
        }
        assert!(matches!(or.recv(), Ok(None)));
        assert!(base.join(row.tar_member_name().expect("cid")).is_file());
    }

    #[test]
    fn materialize_worker_places_then_sends_none() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let id = world.add_place_file("/src/a.bin", 3, 1, FileType::File);
        world.put_payload(id, b"abc");
        let can = world.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row");
        let out = mk_out(1, &root.join("a.bin").to_string_lossy());
        let w = mk_work(can, out.clone(), None);
        let (s, r) = bounded::<MaterializeWork>(4);
        let (os, or) = bounded::<Option<MaterializeOutcome>>(4);
        s.send(w).expect("send");
        drop(s);
        materialize_worker(MaterializeKind::Files, world.config.clone(), None,
                           Shutdown::detached(), r, os);
        match or.recv().expect("recv") {
            Some(Ok(res)) => assert!(res.placed),
            other => { let _ = other; panic!("expected placed outcome") },
        }
        assert!(matches!(or.recv(), Ok(None)));
        assert!(out.abs_path.is_file());
    }

    #[test]
    fn materialize_worker_force_breaks_and_sends_none() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let id = world.add_place_file("/src/a.bin", 3, 1, FileType::File);
        world.put_payload(id, b"abc");
        let can = world.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row");
        let out = mk_out(1, &root.join("a.bin").to_string_lossy());
        let w = mk_work(can, out, None);
        let (s, r) = bounded::<MaterializeWork>(4);
        let (os, or) = bounded::<Option<MaterializeOutcome>>(4);
        let sh = Shutdown::detached();
        sh.request_force();
        s.send(w).expect("send");
        drop(s);
        materialize_worker(MaterializeKind::Files, world.config.clone(), None, sh, r, os);
        assert!(matches!(or.recv(), Ok(None)), "force must skip in-flight work");
    }

    // ---------------------------------------------------------------------
    // materialize_link_tree
    // ---------------------------------------------------------------------

    #[test]
    fn link_tree_absolute_links_symlink_to_source() {
        let world = ExtractWorld::place_test(|c| {
            c.placement.link_tree = true;
            c.placement.absolute_links = true;
        });
        let root = world.ensure_root();
        let base = root.join(".sources");
        fs::create_dir_all(&base).expect("base");
        let id = world.add_place_file("/src/a.bin", 3, 1, FileType::File);
        let cid = content_id_from_digest(Some(&[1; 20]), 3, id, Path::new("f.bin")).0;
        fs::write(base.join(&cid), b"abc").expect("write payload");
        let out_id = world.add_out(&root.join("a.bin").to_string_lossy(), id, None);
        world.db.set_file_flag(id, FileFlag::AtLinkSource, true).expect("at link source");
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        materialize_link_tree(&world.rt(), &mut recorder).expect("link tree");
        assert_eq!(fs::read_link(root.join("a.bin")).expect("readlink"), base.join(&cid));
        assert!(world.out_flag(out_id, OutTreeFlag::Placed));
    }

    #[test]
    fn link_tree_relative_links_resolve_under_root() {
        let world = ExtractWorld::place_test(|c| c.placement.link_tree = true);
        let root = world.ensure_root();
        let base = root.join(".sources");
        fs::create_dir_all(&base).expect("base");
        let id = world.add_place_file("/src/a.bin", 3, 1, FileType::File);
        let cid = content_id_from_digest(Some(&[1; 20]), 3, id, Path::new("f.bin")).0;
        fs::write(base.join(&cid), b"abc").expect("write payload");
        // out at root/sub/a.bin -> relative target resolves to root/.sources/cid
        let sub = root.join("sub");
        fs::create_dir_all(&sub).expect("sub");
        world.add_out(&sub.join("a.bin").to_string_lossy(), id, None);
        world.db.set_file_flag(id, FileFlag::AtLinkSource, true).expect("at link source");
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        materialize_link_tree(&world.rt(), &mut recorder).expect("link tree");
        let link = fs::read_link(sub.join("a.bin")).expect("readlink");
        assert_eq!(link, PathBuf::from("../.sources").join(&cid));
        assert!(sub.join("../.sources").join(&cid).exists());
    }

    #[test]
    fn link_tree_hard_links_use_same_inode() {
        use std::os::unix::fs::MetadataExt;
        let world = ExtractWorld::place_test(|c| {
            c.placement.link_tree = true;
            c.placement.use_hard_links = true;
        });
        let root = world.ensure_root();
        let base = root.join(".sources");
        fs::create_dir_all(&base).expect("base");
        let id = world.add_place_file("/src/a.bin", 3, 1, FileType::File);
        let cid = content_id_from_digest(Some(&[1; 20]), 3, id, Path::new("f.bin")).0;
        fs::write(base.join(&cid), b"abc").expect("write payload");
        let out_id = world.add_out(&root.join("a.bin").to_string_lossy(), id, None);
        world.db.set_file_flag(id, FileFlag::AtLinkSource, true).expect("at link source");
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        materialize_link_tree(&world.rt(), &mut recorder).expect("link tree");
        let src_ino = fs::metadata(base.join(&cid)).expect("meta").ino();
        assert_eq!(fs::metadata(root.join("a.bin")).expect("meta").ino(), src_ino);
        assert!(world.out_flag(out_id, OutTreeFlag::Placed));
    }

    #[test]
    fn link_tree_existing_path_errors() {
        let world = ExtractWorld::place_test(|c| c.placement.link_tree = true);
        let root = world.ensure_root();
        let base = root.join(".sources");
        fs::create_dir_all(&base).expect("base");
        let id = world.add_place_file("/src/a.bin", 3, 1, FileType::File);
        let cid = content_id_from_digest(Some(&[1; 20]), 3, id, Path::new("f.bin")).0;
        fs::write(base.join(&cid), b"abc").expect("write payload");
        let dst = root.join("a.bin");
        fs::write(&dst, b"pre-existing").expect("write dst");
        world.add_out(&dst.to_string_lossy(), id, None);
        world.db.set_file_flag(id, FileFlag::AtLinkSource, true).expect("at link source");
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        let res = materialize_link_tree(&world.rt(), &mut recorder);
        assert!(matches!(res, Err(Error::Config(_))), "Link Tree must be empty");
    }

    // ---------------------------------------------------------------------
    // copy_canonicals_to_source
    // ---------------------------------------------------------------------

    #[test]
    fn copy_canonicals_moves_payloads_and_flags() {
        let world = ExtractWorld::place_test(|c| c.placement.link_tree = true);
        let root = world.ensure_root();
        let id = world.add_place_file("/src/a.bin", 3, 1, FileType::File);
        let cid = world.put_payload(id, b"abc");
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        copy_canonicals_to_source(&world.rt(), &mut recorder).expect("move");
        let dst = root.join(".sources").join(cid.file_name().expect("name"));
        assert_eq!(fs::read(&dst).expect("read"), b"abc");
        assert!(!cid.exists(), "cache removed after move");
        assert!(world.file_flag(id, FileFlag::AtLinkSource));
    }

    #[test]
    fn copy_canonicals_custom_link_source() {
        let world = ExtractWorld::place_test(|c| {
            c.placement.link_tree = true;
            c.placement.link_source = Some(PathBuf::from("links"));
        });
        let root = world.ensure_root();
        let id = world.add_place_file("/src/a.bin", 3, 1, FileType::File);
        let cid = world.put_payload(id, b"abc");
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        copy_canonicals_to_source(&world.rt(), &mut recorder).expect("move");
        assert_eq!(fs::read(root.join("links").join(cid.file_name().expect("name"))).expect("read"),
                   b"abc");
    }

    #[test]
    fn copy_canonicals_no_pending_drops_queue() {
        let world = ExtractWorld::place_test(|c| c.placement.link_tree = true);
        let root = world.ensure_root();
        fs::create_dir_all(&root).expect("root");
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        copy_canonicals_to_source(&world.rt(), &mut recorder).expect("move");
        assert!(world.db.drop_canonical_move_queue().is_ok(), "queue dropped");
    }

    // ---------------------------------------------------------------------
    // prepare_extraction_dir
    // ---------------------------------------------------------------------

    #[test]
    fn prepare_extraction_dir_builds_dir_tree() {
        let world = ExtractWorld::place_test(|c| c.placement.no_create_dir = false);
        world.db.set_out_tree_built().expect("out tree built");
        let root = world.ensure_root();
        let d1 = world.add_out_dir(&root.join("a").to_string_lossy());
        let d2 = world.add_out_dir(&root.join("a/b").to_string_lossy());
        let sid = world.db.add_get_source(Path::new("/x"), "test", Some(1),
                                          Path::new("/x"), Path::new("/x"), Path::new("/x"),
                                          crate::db::flags::SourceFlags::default(),
        ).expect("source");
        world.db.insert_ref_out_rows(&[(d1, sid), (d2, sid)]).expect("ref out");
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        prepare_extraction_dir(&world.rt(), &mut recorder).expect("prepare");
        assert!(root.join("a/b").is_dir());
        assert!(world.db.dir_tree_is_built().expect("meta"));
        assert_eq!(recorder.buffered(), 0);
    }

    #[test]
    fn prepare_extraction_dir_force_interrupts() {
        let world = ExtractWorld::place_test(|c| c.placement.no_create_dir = false);
        world.db.set_out_tree_built().expect("out tree built");
        let root = world.ensure_root();
        let d1 = world.add_out_dir(&root.join("a").to_string_lossy());
        let sid = world.db.add_get_source(Path::new("/x"), "test", Some(1),
                                          Path::new("/x"), Path::new("/x"), Path::new("/x"),
                                          crate::db::flags::SourceFlags::default(),
        ).expect("source");
        world.db.insert_ref_out_rows(&[(d1, sid)]).expect("ref out");
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        let sh = Shutdown::detached();
        sh.request_force();
        let res = prepare_extraction_dir(&world.rt_with(&sh), &mut recorder);
        assert!(matches!(res, Err(Error::Interrupted)));
    }

    // ---------------------------------------------------------------------
    // materialize_loop (3 passes)
    // ---------------------------------------------------------------------

    #[test]
    fn materialize_loop_three_passes() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        // canonical file + dup + symlink
        let cid_f = world.add_place_file("/src/canon.bin", 3, 1, FileType::File);
        world.put_payload(cid_f, b"abc");
        let out_f = world.add_out(&root.join("canon.bin").to_string_lossy(), cid_f, None);
        world.db.with_transaction(|conn| {
            conn.execute("UPDATE out_tree SET canonical_id = id WHERE id = :id",
                         named_params! { ":id": out_f.0 }).expect("self canonical");
            Ok(())
        }).expect("canon tx");
        let cid_d = world.add_dup_file("/src/dup.bin", cid_f);
        let out_d = world.add_out(&root.join("dup.bin").to_string_lossy(), cid_d, Some(out_f));
        let cid_l = world.add_place_symlink("/src/link", "/tgt");
        let out_l = world.add_out(&root.join("link").to_string_lossy(), cid_l, None);

        let mut recorder = Recorder::new(&world.db, true, Some(0));
        materialize_files(&world.rt(), &mut recorder).expect("files");
        materialize_hardlinks(&world.rt(), &mut recorder).expect("hardlinks");
        materialize_others(&world.rt(), &mut recorder).expect("others");

        assert_eq!(fs::read(root.join("canon.bin")).expect("read"), b"abc");
        assert!(world.out_flag(out_f, OutTreeFlag::Placed));
        assert!(world.out_flag(out_d, OutTreeFlag::Placed));
        assert!(world.out_flag(out_d, OutTreeFlag::IsHardlink));
        assert_eq!(fs::read_link(root.join("link")).expect("readlink"), PathBuf::from("/tgt"));
        assert!(world.out_flag(out_l, OutTreeFlag::Placed));
    }

    #[test]
    fn pull_materialize_queue_orders_size_desc() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let small = world.add_place_file("/src/small.bin", 5, 1, FileType::File);
        let large = world.add_place_file("/src/large.bin", 100, 2, FileType::File);
        world.put_payload(small, b"12345");
        world.put_payload(large, &vec![b'x'; 100]);
        let out_s = world.add_out(&root.join("small.bin").to_string_lossy(), small, None);
        let out_l = world.add_out(&root.join("large.bin").to_string_lossy(), large, None);
        world.db.with_transaction(|conn| {
            for id in [out_s.0, out_l.0] {
                conn.execute("UPDATE out_tree SET canonical_id = id WHERE id = :id",
                             named_params! { ":id": id }).expect("self canonical");
            }
            Ok(())
        }).expect("canon tx");
        world.db.create_materialize_queue().expect("create queue");
        world.db.populate_materialize_queue().expect("populate queue");
        let rows = world.db.pull_materialize_queue::<FileRecord>(0, 10).expect("pull");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, 1, "size DESC: large first");
        assert_eq!(rows[0].1.size, 100);
        assert_eq!(rows[1].1.size, 5);
        assert_eq!(rows[0].2.abs_path, root.join("large.bin"));
    }

    // ---------------------------------------------------------------------
    // run()
    // ---------------------------------------------------------------------

    fn seed_run_files(world: &ExtractWorld) -> (OutTreeId, OutTreeId, OutTreeId) {
        let root = world.root();
        let cid_f = world.add_place_file("/src/canon.bin", 3, 1, FileType::File);
        world.put_payload(cid_f, b"abc");
        let out_f = world.add_out(&root.join("canon.bin").to_string_lossy(), cid_f, None);
        world.db.with_transaction(|conn| {
            conn.execute("UPDATE out_tree SET canonical_id = id WHERE id = :id",
                         named_params! { ":id": out_f.0 }).expect("self canonical");
            Ok(())
        }).expect("canon tx");
        let cid_d = world.add_dup_file("/src/dup.bin", cid_f);
        let out_d = world.add_out(&root.join("dup.bin").to_string_lossy(), cid_d, Some(out_f));
        let cid_l = world.add_place_symlink("/src/link", "/tgt");
        let out_l = world.add_out(&root.join("link").to_string_lossy(), cid_l, None);
        world.db.set_placement_prologue_done().expect("prologue done");
        (out_f, out_d, out_l)
    }

    #[test]
    fn run_places_files_hardlinks_and_others() {
        use std::os::unix::fs::MetadataExt;
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let (out_f, out_d, out_l) = seed_run_files(&world);
        run(&world.rt()).expect("run");

        assert_eq!(fs::read(root.join("canon.bin")).expect("read"), b"abc");
        assert_eq!(fs::read(root.join("dup.bin")).expect("read"), b"abc");
        assert_eq!(fs::metadata(root.join("canon.bin")).expect("meta").ino(),
                   fs::metadata(root.join("dup.bin")).expect("meta").ino(),
                   "dup must be a hardlink of the canonical");
        assert_eq!(fs::read_link(root.join("link")).expect("readlink"), PathBuf::from("/tgt"));
        assert!(!world.config.paths.extract_cache_dir().exists(),
                "cache removed by cleanup_stage");
        assert!(world.out_flag(out_f, OutTreeFlag::Placed));
        assert!(world.out_flag(out_d, OutTreeFlag::Placed));
        assert!(world.out_flag(out_d, OutTreeFlag::IsHardlink));
        assert!(world.out_flag(out_l, OutTreeFlag::Placed));
        let phase = world.db.get_file_by_id::<StrippedRecord>(
            world.db.file_id_by_abs_path(Path::new("/src/canon.bin")).expect("lookup").expect("row")
        ).expect("get").expect("row").phase;
        assert_eq!(phase, FilePhase::AtDestination);
    }

    #[test]
    fn run_clean_target_with_one_top_level() {
        let world = ExtractWorld::place_test(|c| {
            c.placement.clean_target = true;
            c.placement.one_top_level = Some(PathBuf::from("otl"));
            c.placement.no_create_dir = false;
        });
        let root = world.ensure_root();
        // Step 1.1 wipes the *effective* root; it must exist for remove_dir_all.
        fs::create_dir_all(root.join("otl")).expect("otl root");
        fs::write(root.join("otl/stale.txt"), b"stale").expect("write stale");
        let id = world.add_place_file("/src/a.bin", 3, 1, FileType::File);
        world.put_payload(id, b"abc");
        world.add_out_dir(&root.join("otl").to_string_lossy());
        let out_f = world.add_out(&root.join("otl/a.bin").to_string_lossy(), id, None);
        world.db.with_transaction(|conn| {
            conn.execute("UPDATE out_tree SET canonical_id = id WHERE id = :id",
                         named_params! { ":id": out_f.0 }).expect("self canonical");
            Ok(())
        }).expect("canon tx");
        world.db.set_out_tree_built().expect("out tree built");
        world.db.set_placement_prologue_done().expect("prologue done");
        run(&world.rt()).expect("run");
        assert!(!root.join("otl/stale.txt").exists(), "clean-target removed stale entry");
        assert_eq!(fs::read(root.join("otl/a.bin")).expect("read"), b"abc");
    }

    #[test]
    fn run_clean_target_without_one_top_level_is_noop() {
        let world = ExtractWorld::place_test(|c| c.placement.clean_target = true);
        let root = world.ensure_root();
        fs::write(root.join("stale.txt"), b"stale").expect("write stale");
        let id = world.add_place_file("/src/a.bin", 3, 1, FileType::File);
        world.put_payload(id, b"abc");
        let out_f = world.add_out(&root.join("a.bin").to_string_lossy(), id, None);
        world.db.with_transaction(|conn| {
            conn.execute("UPDATE out_tree SET canonical_id = id WHERE id = :id",
                         named_params! { ":id": out_f.0 }).expect("self canonical");
            Ok(())
        }).expect("canon tx");
        world.db.set_placement_prologue_done().expect("prologue done");
        run(&world.rt()).expect("run");
        assert!(root.join("stale.txt").exists(), "clean-target requires --one-top-level");
        assert_eq!(fs::read(root.join("a.bin")).expect("read"), b"abc");
    }

    #[test]
    fn run_empty_out_tree_is_ok() {
        let world = ExtractWorld::place_test(|_| {});
        world.ensure_root();
        world.db.set_placement_prologue_done().expect("prologue done");
        run(&world.rt()).expect("run");
    }

    #[test]
    fn run_link_tree_smoke() {
        let world = ExtractWorld::place_test(|c| c.placement.link_tree = true);
        let root = world.ensure_root();
        let cid_f = world.add_place_file("/src/canon.bin", 3, 1, FileType::File);
        world.put_payload(cid_f, b"abc");
        let out_f = world.add_out(&root.join("canon.bin").to_string_lossy(), cid_f, None);
        world.db.set_placement_prologue_done().expect("prologue done");
        run(&world.rt()).expect("run");
        // Only moved (canonical, `FileExtracted`) entries are linked; the target
        // is the canonical's content id in the link source.
        let rec = world.db.get_file_by_id::<FileRecord>(cid_f).expect("get").expect("row");
        let source = root.join(".sources").join(rec.content_id().expect("cid").0);
        // The link is written relative to the extraction root; it must resolve.
        assert_eq!(fs::canonicalize(root.join("canon.bin")).expect("resolve"),
                   fs::canonicalize(&source).expect("source exists"));
        assert!(world.out_flag(out_f, OutTreeFlag::Placed));
        assert!(!world.config.paths.extract_cache_dir().exists(), "cache removed");
    }

    // ---------------------------------------------------------------------
    // cleanup_stage
    // ---------------------------------------------------------------------

    fn cache_as_file(world: &ExtractWorld) -> PathBuf {
        let cache = world.config.paths.extract_cache_dir();
        fs::remove_dir_all(&cache).expect("remove cache dir");
        fs::write(&cache, b"not a dir").expect("write file");
        cache
    }

    #[test]
    fn cleanup_stage_fail_fast_errors_on_undir() {
        let world = ExtractWorld::place_test(|c| c.process.fail_fast = true);
        cache_as_file(&world);
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        let res = cleanup_stage(&world.rt(), &mut recorder);
        assert!(matches!(res, Err(Error::Config(_))));
    }

    #[test]
    fn cleanup_stage_non_fatal_warns_on_undir() {
        let world = ExtractWorld::place_test(|_| {});
        cache_as_file(&world);
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        cleanup_stage(&world.rt(), &mut recorder).expect("ok");
    }

    #[test]
    fn cleanup_stage_keeps_cache_when_keeping_stage() {
        let world = ExtractWorld::place_test(|c| c.process.cleanup.keep_stage = true);
        let cache = world.config.paths.extract_cache_dir();
        fs::write(cache.join("x"), b"x").expect("write");
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        cleanup_stage(&world.rt(), &mut recorder).expect("ok");
        assert!(cache.is_dir(), "cache kept");
    }

    // ---------------------------------------------------------------------
    // Integration: hardlink groups on a real filesystem
    // ---------------------------------------------------------------------

    /// Real-disk hardlink pair: `link_a` and `link_b` share an inode.
    fn disk_hardlink_pair(world: &ExtractWorld) -> (PathBuf, PathBuf) {
        let a = world.path("src/link_a");
        let b = world.path("src/sub/link_b");
        fs::create_dir_all(b.parent().expect("parent")).expect("dirs");
        fs::write(&a, b"hard").expect("write a");
        fs::hard_link(&a, &b).expect("hardlink b");
        (a, b)
    }

    /// Seed files/out rows for a two-file same-inode group and run the given
    /// canonical election; returns (out_a, out_b) ids.
    /// `disjoint`: each out row belongs to exactly one source (`a` in s1, `b`
    /// in s2); otherwise `b` is shared by both sources (overlapping trees).
    fn seed_hardlink_group(
        world: &ExtractWorld,
        sources: bool,
        disjoint: bool,
        mark: impl FnOnce(&ExtractWorld) -> ()) -> (OutTreeId, OutTreeId) {
        use std::os::unix::fs::MetadataExt;
        let (a, _b) = disk_hardlink_pair(world);
        let md_a = fs::metadata(&a).expect("meta");
        let fa = world.add_place_file_inode("/src/link_a", 4, 1, FileType::File,
                                      Some(md_a.dev()), Some(md_a.ino()));
        let fb = world.add_place_file_inode("/src/sub/link_b", 4, 2, FileType::File,
                                      Some(md_a.dev()), Some(md_a.ino()));
        let root = world.root();
        let out_a = world.add_out(&root.join("link_a").to_string_lossy(), fa, None);
        let out_b = world.add_out(&root.join("sub/link_b").to_string_lossy(), fb, None);
        fs::create_dir_all(root.join("sub")).expect("dest sub dir");
        if sources {
            let s1 = world.db.add_get_source(Path::new("/src"), "test", Some(1),
                                             Path::new("/src"), Path::new("/src"), Path::new("/src"),
                                             crate::db::flags::SourceFlags::default()
                                                 .with(crate::db::flags::SourceFlag::IsDirectory, true),
            ).expect("source");
            let s2 = world.db.add_get_source(Path::new("/src/sub"), "test", Some(1),
                                             Path::new("/src/sub"), Path::new("/src/sub"), Path::new("/src/sub"),
                                             crate::db::flags::SourceFlags::default()
                                                 .with(crate::db::flags::SourceFlag::IsDirectory, true),
            ).expect("source");
            let refs: &[(crate::db::types::OutTreeId, i64)] = if disjoint {
                &[(out_a, s1), (out_b, s2)]
            } else {
                &[(out_a, s1), (out_b, s1), (out_b, s2)]
            };
            world.db.insert_ref_out_rows(refs).expect("ref out");
        }
        // cache payloads for both self-canonical content rows
        world.put_payload(fa, b"hard");
        world.put_payload(fb, b"hard");
        mark(world);
        (out_a, out_b)
    }

    fn materialize_group(world: &ExtractWorld) {
        let mut recorder = Recorder::new(&world.db, true, Some(0));
        materialize_files(&world.rt(), &mut recorder).expect("files");
        materialize_hardlinks(&world.rt(), &mut recorder).expect("hardlinks");
    }

    fn canonical_of(world: &ExtractWorld, out: OutTreeId) -> Option<OutTreeId> {
        world.db.with_transaction(|conn| {
            let v: Option<i64> = conn.query_row(
                "SELECT canonical_id FROM out_tree WHERE id = :id",
                named_params! { ":id": out.0 }, |r| r.get(0),
            ).unwrap_or(None);
            Ok(v.map(OutTreeId))
        }).expect("read canonical")
    }

    #[test]
    fn integration_global_links_shared_inode() {
        use std::os::unix::fs::MetadataExt;
        let world = ExtractWorld::place_test(|_| {});
        world.ensure_root();
        let (out_a, out_b) = seed_hardlink_group(&world, true, false, |w| {
            w.db.mark_global_canonical().expect("global");
        });
        assert_eq!(canonical_of(&world, out_b), Some(out_a),
                   "same-inode group elected a single canonical out row");
        materialize_group(&world);
        let root = world.root();
        let md_a = fs::metadata(root.join("link_a")).expect("meta");
        let md_b = fs::metadata(root.join("sub/link_b")).expect("meta");
        assert_eq!(md_a.ino(), md_b.ino(), "materialized group is a real hardlink");
        assert!(world.out_flag(out_b, OutTreeFlag::IsHardlink));
    }

    #[test]
    fn integration_source_grouping_keeps_sources_disjoint() {
        use std::os::unix::fs::MetadataExt;
        let world = ExtractWorld::place_test(|_| {});
        world.ensure_root();
        let (out_a, out_b) = seed_hardlink_group(&world, true, true, |w| {
            // Per-source grouping: each source holds exactly one member, so no
            // cross-source link is elected.
            let sids = w.db.list_sources(None, 0, 100).expect("sources");
            for s in &sids {
                w.db.mark_source_canonical(s.id).expect("source canonical");
            }
        });
        assert_eq!(canonical_of(&world, out_a), Some(out_a));
        assert_eq!(canonical_of(&world, out_b), Some(out_b),
                   "disjoint sources must not cross-link");
        materialize_group(&world);
        let root = world.root();
        let md_a = fs::metadata(root.join("link_a")).expect("meta");
        let md_b = fs::metadata(root.join("sub/link_b")).expect("meta");
        assert_ne!(md_a.ino(), md_b.ino(), "separate copies, not a hardlink");
        assert!(!world.out_flag(out_b, OutTreeFlag::IsHardlink));
    }

    #[test]
    fn integration_none_grouping_never_links() {
        use std::os::unix::fs::MetadataExt;
        let world = ExtractWorld::place_test(|_| {});
        world.ensure_root();
        let (out_a, out_b) = seed_hardlink_group(&world, true, false, |w| {
            w.db.mark_all_canonical().expect("all canonical");
        });
        assert_eq!(canonical_of(&world, out_a), Some(out_a));
        assert_eq!(canonical_of(&world, out_b), Some(out_b));
        materialize_group(&world);
        let root = world.root();
        let md_a = fs::metadata(root.join("link_a")).expect("meta");
        let md_b = fs::metadata(root.join("sub/link_b")).expect("meta");
        assert_ne!(md_a.ino(), md_b.ino(), "None grouping must never link");
    }

    #[test]
    fn integration_dir_and_symlink_not_grouped() {
        let world = ExtractWorld::place_test(|_| {});
        let root = world.ensure_root();
        let d = root.join("d");
        fs::create_dir_all(&d).expect("dir");
        let idir = world.add_place_file_inode("/d", 0, 9, FileType::Directory, Some(1), Some(500));
        let sa = root.join("sa");
        std::os::unix::fs::symlink(&d, &sa).expect("symlink");
        let isa = world.add_place_file_inode("/sa", 0, 9, FileType::Symlink(LinkType::Directory),
                                       Some(1), Some(501));
        let out_dir = world.add_out_dir(&root.join("d").to_string_lossy());
        let out_sa = world.add_out(&root.join("sa").to_string_lossy(), isa, None);
        let _ = idir;
        world.db.mark_global_canonical().expect("global");
        assert_eq!(canonical_of(&world, out_dir), None, "dir rows never canonical");
        assert_eq!(canonical_of(&world, out_sa), None, "symlink rows never grouped");
    }
}
