//! Scan/untar: footer-first catalog, else leading `manifest.sqlite`; cache payloads.

use crate::archive_footer::{read_footer, verify_archive_integrity};
use crate::common::defaults::{SNAPSHOT_INIT_TAR_NAME, SNAPSHOT_TAR_NAME};
use crate::common::files::copy_reader_batched;
use crate::config::ExtractConfig;
use crate::db::content_id::parse_content_id;
use crate::db::flags::{ErrorFlags, FileFlag};
use crate::db::types::{FileId, FilePhase};
use crate::db::{Database, ErrorPhase, Recorder, ScanState};
use crate::error::{Error, FileStatError, Result};
use crate::progress::{BarKind, ProgressBarSet};
use crate::shutdown::Shutdown;
use crate::tar_reader::open_tar_archive;
use crate::unarchive::ExtractRTArgs;
use crate::unarchive::filter::ParseFilterBuffer;
use crate::unarchive::filter::run as filter_run;
use indicatif::ProgressBar;
use path_clean::PathClean;
use std::fs;
use std::io;
use std::io::{BufReader, Read};
use std::path::Path;
use std::time::Duration;
use tar::Entry;

const OPT_DB_ERROR: &str = "INVARIANT ERROR: Database expected to be present at this point";
const ERROR_PHASE: ErrorPhase = ErrorPhase::Extract(crate::config::ExtractPipelinePhase::ScanTar);

struct ScanRTState <'a> {
    pub db: Option<Database>,
    pub config: &'a ExtractConfig,
    pub shutdown: &'a  Shutdown,
    pub progress: &'a  ProgressBarSet,
    pub pb: ProgressBar,
}
// INFO: State Model for a File and its associated canonical row in the db:
//    0. File seen in the stream
//    1. File finished extracting successfully => FileExtracted = True
//    2. Snapshot Encountered => All files with phase='archived' in snapshot && FileExtracted => phase='unarchived'
//       <Report File Counts>
//    4. Promote all FileExtracted = True && phase= 'sparsified' | 'archived' => phase='unarchived'
//    5. Promote remaining entries in DB to 'unarchived'

/// Record a session-scoped scan error. Errors are buffered in the shared
/// speculative [`Recorder`] and persisted at a phase boundary (once a database is
/// available); if the scan aborts first, [`Recorder::flush`]/`Drop` reports the loss.
fn record_session_error(recorder: &mut Recorder, error: FileStatError) {
    recorder.record_session(ERROR_PHASE, error, ErrorFlags::default());
}

/// Record a per-file scan error (e.g. a failed payload `unpack`). The target row
/// is referenced by `file_id`.
fn record_file_scan_error(recorder: &mut Recorder, file_id: FileId, error: FileStatError) {
    recorder.record_file(file_id, ERROR_PHASE, error, ErrorFlags::default());
}

/// Convert a tar-stream io error into the crate `Error`, buffering a session-scoped
/// scan error first (best-effort; a failed record is ignored, the stream error wins).
fn io_error_with_session_scan_error(archive_path: &Path, recorder: &mut Recorder, e: io::Error)
    -> Error {
    let file_stat = FileStatError::copy_io(archive_path, &e);
    record_session_error(recorder, file_stat);
    Error::io(archive_path, e)
}

/// Best-effort persist the buffered scan errors against the pass's database, when
/// one exists. The database is used transiently (no stored reference), so it may
/// be a value that is created and dropped within the scan frame. Without a
/// database, [`Recorder::flush`] reports the loss.
fn flush_scan_recorder(recorder: &mut Recorder, db: Option<&Database>) -> Result<()> {
    match db {
        Some(d) => {
            recorder.flush_into(d)?;
            Ok(())
        }
        None => {
            recorder.flush()?;
            Ok(())
        }
    }
}

/// Account a freshly loaded scan DB against the progress bars. Called after every
/// `rt.db = Some(...)` assignment: the global bar is sized to the full table
/// (`count_entries`, later refined by place_prologue), the phase bar is scoped to
/// the encoded payloads (one tar member each) and switched from spinner to a
/// fraction bar, non-encoded rows are promoted eagerly (`inc_global`), and rows
/// already extracted in a prior pass seed the phase position (`inc_both`).
/// When `announce` is `Some((resume_db, last_processed_member_index))` — the
/// initial catalog load only; the mid-scan force install passes `None` — the
/// "starting scan" log line is emitted with the real total/processing/pending.
fn attach_db_progress(
    db: &Database,
    progress: &ProgressBarSet,
    announce: Option<(bool, u64)>)
    -> Result<()> {

    let total = db.count_entries()?;
    let processing = db.count_encoded_payloads()?;
    let extracted = db.count_extracted_canonical()?;

    progress.set_table_size(total);
    progress.set_phase_kind(BarKind::Count);
    progress.set_phase_total(processing);
    progress.inc_global(total.saturating_sub(processing));
    progress.inc_both(extracted);

    if let Some((resume_db, last_processed)) = announce {
        let pending = processing.saturating_sub(extracted);
        if resume_db {
            tracing::info!(
                "starting scan: total {total}, processing {processing}, pending {pending} \
                 (resume from member {})",
                last_processed + 1, // Human readable
            );
        } else {
            tracing::info!(
                "starting scan: total {total}, processing {processing}, pending {pending}"
            );
        }
    }
    Ok(())
}

/// Drain the filter_buffer and write it to the db.
fn store_filter_buffer(
    fb: &mut Option<ParseFilterBuffer>, rt: &ExtractRTArgs)
    -> Result<u64> {
    let result = fb
        .as_ref()
        .expect("PRECONDITION FAILED: Some filter_buffer expected")
        .write_to_db(
            Box::new(|from, line, query|
                rt.db.add_include_pattern_extract(from, line, query)),
            Box::new(|from, line, query|
                rt.db.add_exclude_pattern_extract(from, line, query)))?;

    tracing::info!("Flushed: {result} filter rows to db");
    *fb = None;
    if rt.config.filter.eager_filter {
        // Dedicated `filter` sub-bar (stacked above the scan bar) for the eager
        // filter — including the trivial fast path, which still advances
        // progress (inc_both on the filter's row count). The guard routes the
        // shared filter progress to the sub-bar and removes it on drop; the scan
        // bar and the global stay untouched. `filter_run` itself no-ops when the
        // filters were already applied (resume), in which case the bar never
        // advances and is simply removed here.
        let fbar = rt.progress.push_sub_bar("filter", BarKind::Counter);
        let _guard = rt.progress.redirect_phase(fbar);
        filter_run(rt, true)?;
    }
    Ok(result)
}

/// Walk the tar stream: load catalog (footer or leading manifest), cache payloads, promote.
/// POSTCONDITION filter_buffer contract: Function takes buffer, drains it to db and sets the
///  buffer to None, to save memory.
pub fn run(
    config: &ExtractConfig,
    db_path: &Path,
    shutdown: &Shutdown,
    recorder: &mut Recorder,
    filter_buffer: &mut Option<ParseFilterBuffer>,
    progress: &ProgressBarSet)
    -> Result<Database> {

    let rt = ScanRTState {
        config,
        db: None,
        shutdown,
        progress,
        pb: progress.push_sub_bar("extracting", BarKind::Bytes),
    };
    run_inner(rt, db_path, recorder, filter_buffer)
}


fn run_inner(
    outer_rt: ScanRTState,
    db_path: &Path,
    recorder: &mut Recorder,
    filter_buffer: &mut Option<ParseFilterBuffer>) -> Result<Database>  {

    let mut rt = outer_rt;
    let mut force_buffer: Option<(Vec<FileId>, Vec<FileId>)> = if rt.config.process.force {
        Some((Vec::new(), Vec::new()))
    } else {
        None
    };

    // INFO: Noop if dir exists!
    // INFO: If we can't create an extract stage, we can only abort.
    fs::create_dir_all(rt.config.paths.extract_cache_dir())
        .map_err(|e| Error::io(&rt.config.paths.extract_cache_dir(), e))?;

    let resume_db = db_path.is_file();
    // Only a first pass installs the footer catalog; later passes inherit the fact
    // that it came from a footer through `ExtractScanState::from_footer`.
    // INFO: read into a temp file first; installed below via rename to `db_path`.
    // A footer structure present but failing to extract is a real anomaly: record
    // it as a session error and fall back to footer-less handling.
    let footer = match read_footer(&rt.config.paths.archive_path, &rt.config.paths.temp_db()) {
        Ok(present) => present,
        Err(e @ Error::FileStat(_)) => {
            record_session_error(
                recorder,
                e.to_file_stat(Some(&rt.config.paths.archive_path)),
            );
            false
        },
        Err(e) => return Err(e)
    };
    let footer_this_pass = !resume_db && footer;

    // Verify the integrity of the archive by the footer sha, if the sha is present
    if footer_this_pass {
        match verify_archive_integrity(&rt.config.paths.archive_path) {
            Ok(true) => (),
            Ok(false) => {
                let msg = format!(
                    "archive content sha1 mismatch (footer vs stream) in {}",
                    rt.config.paths.archive_path.display()
                );
                record_session_error(recorder, FileStatError::general(None, msg.clone()));
                if rt.config.process.force {
                    tracing::warn!("{msg}; continuing (--force)");
                } else {
                    return Err(Error::Config(msg));
                }
            }
            Err(e) => {
                record_session_error(
                    recorder,
                    e.to_file_stat(Some(&rt.config.paths.archive_path)),
                );
                if rt.config.process.force {
                    tracing::warn!("archive integrity check failed; continuing (--force)");
                } else {
                    return Err(e);
                }
            }
        }
    }

    rt.db = if resume_db {
        open_resume_database(&db_path)?
    } else if footer_this_pass {
        Some(open_initial_database(&rt, db_path, filter_buffer)?)
    } else {
        None
    };

    let mut scan = if let Some(edb) = rt.db.as_ref() {
        let s = edb.load_extract_scan_state()?;
        attach_db_progress(edb, rt.progress, Some((resume_db, s.last_member_index)))?;
        recorder.set_flush_limit(rt.config.process.auto_flush_limit);
        s
    } else {
        tracing::info!("starting scan: no catalog yet (awaiting manifest)");
        let mut scn = ScanState::default();
        scn.from_footer |= footer_this_pass;
        scn
    };
    scan.from_footer |= footer_this_pass;

    let mut archive = open_tar_archive(
        &rt.config.paths.archive_path, &rt.config.decompression
    )?;

    // Resume skip bound: members up to **and including** the last one fully
    // processed by an earlier pass are skipped
    let skip_processed_until = scan.saw_any_members.then_some(scan.last_member_index);

    // FEATURE: Switch to seek for tar
    for (m_idx, wrapped_entry) in archive
        .entries()
        .map_err(|e| {
            io_error_with_session_scan_error(&rt.config.paths.archive_path, recorder, e)
        })?
        .enumerate() {

        if rt.shutdown.is_interrupted() {
            scan.nt_stopped = true;
            break;
        }

        // INFO: iterating entries will lead to the body being consumed too (no copy to sink needed)
        let member_index = m_idx as u64;
        if skip_processed_until.is_some_and(|last| member_index <= last) {
            // INFO: Still mapping error to record issues when recovering and file got corrupted
            //  midway
            wrapped_entry.map_err(|e| {
                io_error_with_session_scan_error(&rt.config.paths.archive_path, recorder, e)
            })?;
            continue;
        }

        let mut entry = wrapped_entry
            .map_err(|e| io_error_with_session_scan_error(
                &rt.config.paths.archive_path, recorder, e
            ))?;
        // INFO: On Unix, this should never produce an error
        //  On Windows, this causes an error, if the path is not valid utf-8
        let path = match entry
            .path()
            .map_err(|e| io_error_with_session_scan_error(
                &rt.config.paths.archive_path, recorder, e
            )) {
            Ok(p) => p,
            Err(_) => continue,
        };
        // Member names MUST follow our convention (content id / catalog name) and
        // hence be UTF-8. A non-UTF8 name is lossy-converted so that it provably
        // fails `parse_content_id` and lands in the `(other, _)` reject arm below.
        let name = match entry_name(&path) {
            Ok(n) => n,
            Err(e) => {
                record_session_error(recorder, e.to_file_stat(Some(&path)));
                return Err(e);
            }
        };

        tracing::debug!("Index: {} Path: {}", member_index, path.display());
        process_entry(
            &mut rt,
            db_path,
            &name,
            &mut entry,
            &mut force_buffer,
            &mut scan,
            recorder,
            filter_buffer)?;

        scan.saw_any_members = true;
        scan.last_member_index = member_index;
        scan.last_member_was_snapshot = name == SNAPSHOT_TAR_NAME;
    }

    // Persist observations before any early return / interrupt propagation.
    if let Some(d) = rt.db.as_ref() {
        d.save_extract_scan_state(&scan)?;
        d.checkpoint()?;
    }

    if scan.nt_stopped {
        // Best-effort persist whatever was buffered against the pass's database.
        flush_scan_recorder(recorder, rt.db.as_ref())?;
        return Err(Error::Interrupted);
    }

    if !scan.saw_any_members {
        flush_scan_recorder(recorder, rt.db.as_ref())?;
        return Err(Error::Config("Archive is Empty".to_string()));
    }

    // PRECONDITION: nt_stopped == false && saw_any_members == true
    // Abort case, we have members but we don't want to keep them
    if rt.db.is_none() {
        // Remove stage if necesssary
        if !rt.config.process.cleanup.keep_stage {
            if let Err(e) = fs::remove_dir_all(rt.config.paths.extract_cache_dir()) {
                io_error_with_session_scan_error(&rt.config.paths.extract_cache_dir(), recorder, e);
            };
        }
        // Nothing to attach the buffered errors to; the flush reports the loss.
        flush_scan_recorder(recorder, rt.db.as_ref())?;
        if rt.config.process.force {
            return Err(Error::Config(
                "Archive did not contain database. Cannot continue extraction".to_string(),
            ));
        } else {
            panic!(
                "INVARIANT ERROR: No Database with at least one canonical file. \
                 MUST NOT happen when force is false"
            );
        }
    }
    // PRECONDITION: nt_stopped == false && saw_any_members == true && db.is_some() == true
    validate_result(&scan, resume_db, rt.config.process.force)?;
    let sdb = rt.db.expect(OPT_DB_ERROR);

    // Check for any payloads that were extracted but somehow not recorded
    let (present, new) = sdb.check_cached_payloads(&rt.config.paths.extract_cache_dir())?;
    if new > 0 {
        let msg = format!(
            "Found {new} archive payload(s) which were not recorded as extracted.");
        if rt.config.process.force {
            tracing::warn!("{msg}");
        } else {
            panic!("{msg}");
        }
    }

    // Deal with unfinished promotion (e.g. snapshot missing) -> Blanket prmote here.
    let n = sdb.promote_extracted_to_unarchived()?;
    if n > 0 {
        tracing::warn!(
            "promoted {n} extracted row(s) to unarchived without snapshot confirmation"
        );
    }

    // On successful exit, all rows should be in 'unarchived' state and all rows with a AppendedPath
    // flag set should have an associated `FileExtracted` or `ErrorWhileExtracting` Flag
    let tally = sdb.tally_scan_remainders(rt.config.filter.eager_filter)?;
    if tally.filter_excluded > 0 {
        let promoted = sdb.promote_filter_excluded_remainders()?;
        debug_assert_eq!(promoted, tally.filter_excluded);
        tracing::info!(
            "scan: {} file(s) excluded by the extract filter (not extracted by design)",
            tally.filter_excluded,
        );
    }

    let violations = tally.violations();
    if violations > 0 {
        let last_member = if scan.last_member_was_snapshot {
            "a snapshot.sqlite (possibly a mid-archive session checkpoint)"
        } else {
            "a non-snapshot member"
        };
        let msg = format!(
            "INVARIANT VIOLATION after scan: {} row(s) not in 'unarchived':\n\
             \x20 - {} file(s) never confirmed by a final snapshot/footer (still 'staged'; \
             last tar member was {last_member}, from_footer={})\n\
             \x20 - {} file(s) failed to extract their payload (ErrorWhileExtracting)\n\
             \x20 - {} file(s) were marked as archived/added but their payload was not \
             found in the archive stream",
            violations,
            tally.staged,
            scan.from_footer,
            tally.extracting_err,
            tally.missing_member,
        );
        if rt.config.process.force {
            tracing::warn!("{msg}");
            record_session_error(recorder, FileStatError::general(None, msg));
            let promoted = sdb.promote_scan_remainders()?;
            debug_assert!(promoted >= violations, "promote count must cover the violations");
            flush_scan_recorder(recorder, Some(&sdb))?;
        } else {
            panic!("{msg}\nRun with --force to proceed (affected files will be skipped).");
        }
    }

    let paths = sdb.count_files_in_phase(FilePhase::Unarchived)?;
    debug_assert_eq!(
        sdb.count_entries()?,
        paths,
        "INVARIANT ERROR: after the scan gate every row must be 'unarchived'"
    );

    tracing::info!(
        "scan done: processed {} of {}, {} error(s)",
        present,
        paths,
        sdb.count_errors(Some("scan_tar"))?,
    );

    if rt.config.paths.temp_db().exists() {
        match fs::remove_file(rt.config.paths.temp_db()) {
            Ok(()) => tracing::info!("Deleted the temp_db"),
            Err(e) => {
                let err = io_error_with_session_scan_error(
                    &rt.config.paths.temp_db(), recorder, e);
                tracing::warn!("Failed to delete temp_db: {err}")
            }
        }
    }

    Ok(sdb)
}

/// Fully process an entry from the tar archive.
/// Precondition: Index is valid (i.e. not extracted yet)
fn process_entry(
    rt: &mut ScanRTState,
    db_path: &Path,
    name: &str,
    entry: &mut Entry<BufReader<Box<dyn Read>>>,
    force_buffer: &mut Option<(Vec<FileId>, Vec<FileId>)>,
    scan: &mut ScanState,
    recorder: &mut Recorder,
    fb: &mut Option<ParseFilterBuffer>)
    -> Result<()> {

    let mut install_db = |rts: &mut ScanRTState| {
        // The in-tar `manifest.sqlite` is authoritative only for footer-less
        // archives: with a footer the catalog was already installed up-front and
        // is the *final* db — overlaying the older in-tar manifest would regress it.
        if !scan.from_footer {
            install_database(rts, db_path, entry, recorder, fb)
        } else {
            Ok(())
        }
    };

    match (name, scan.saw_any_members) {
        // Spec conform: No db, initial snapshot is first.
        (SNAPSHOT_INIT_TAR_NAME, false) => {
            install_db(rt)?;
            scan.saw_manifest_db = true;
        }
        // Not Spec: Abort
        // INFO: No truncated case where initial db is not first element.
        //  So unconditionally rejected.
        (SNAPSHOT_INIT_TAR_NAME, true) => {
            // TODO different error!
            return Err(Error::Config(
                "Found manifest.sqlite not at the beginning of the archive".to_string(),
            ));
        }
        // Not Spec: First Member is Snapshot.
        (SNAPSHOT_TAR_NAME, false) => {
            if !rt.config.process.force {
                return Err(Error::Config(format!(
                    "first tar member is {SNAPSHOT_TAR_NAME} not manifest.sqlite; \
                         attempt to bypass with --force"
                )));
            } else {
                install_db(rt)?;
            }
            let ref_db = rt.db.as_ref().expect(OPT_DB_ERROR);
            scan.snapshots_ingested = ref_db.record_snapshot_ingested()?;
            tracing::info!("snapshot {} applied", scan.snapshots_ingested);
        }
        (SNAPSHOT_TAR_NAME, true) => match (rt.config.process.force, rt.db.is_none()) {
            // Regardless of force state - with a database, apply the snapshot.
            (false, false) | (true, false) => {
                let ldb = rt.db.as_ref().expect(OPT_DB_ERROR);
                // Ingest the snapshot data into the working database.
                captured_extract_database(&rt.config.paths.temp_db(), entry, recorder)?;
                ldb.apply_snapshot_promote_unarchived(&rt.config.paths.temp_db())?;
                scan.snapshots_ingested = ldb.record_snapshot_ingested()?;
                tracing::info!("snapshot {} applied", scan.snapshots_ingested);
            }
            (false, true) => panic!(
                "INVARIANT ERROR: without force, the first member of an archive MUST be an \
                initial db"),
            (true, true) => {
                install_db(rt)?;
                let ref_db = rt.db.as_ref().expect(OPT_DB_ERROR);
                scan.snapshots_ingested = ref_db.record_snapshot_ingested()?;
                tracing::info!("snapshot {} applied", scan.snapshots_ingested);

                // Number of buffered members from force buffer.
                let window_members = match &force_buffer {
                    Some(buf) => buf.0.len() + buf.1.len(),
                    None => 0,
                };

                if let Some(buf) = force_buffer.as_mut() {
                    for fid in buf.0.drain(..) {
                        ref_db.set_file_flag(fid, FileFlag::FileExtracted, true)?;
                    }
                    for fid in buf.1.drain(..) {
                        ref_db.set_file_flag(fid, FileFlag::ErrorWhileExtracting, true)?;
                    }
                }
                // Dump buffered errors into the db.
                flush_scan_recorder(recorder, Some(&ref_db))?;
                // INFO: without db, the phase is only incremented. With db, both are incremented.
                rt.progress.inc_global(window_members as u64);
            }
        }
        (cid, saw_first) if let Ok((_, _, fid, _)) = parse_content_id(cid) => {
            if !rt.config.process.force && !saw_first {
                return Err(Error::Config(format!(
                    "first tar member is canonical file {cid} not manifest.sqlite; \
                     bypass available with --force"
                )));
            }
            extract_canonical_member(force_buffer, entry, recorder, rt, fid, name)?
        }
        (other, _) => {
            return Err(Error::Config(format!(
                "tar member `{other}` is neither a catalog \
                nor a content id; not a tar-dedup archive?"
            )));
        }
    }
    Ok(())
}

/// Provided we have an entry to extract, extract the entry to the computed destination. Uses
/// either batched_copy or sparse_copy
fn extract_canonical_member(
    force_buffer: &mut Option<(Vec<FileId>, Vec<FileId>)>,
    entry: &mut Entry<BufReader<Box<dyn Read>>>,
    recorder: &mut Recorder,
    rt: &ScanRTState,
    fid: FileId,
    name: &str)
    -> Result<()> {
    // Pre-DB window (--force, no catalog yet): advance only the phase bar so it
    // counts payload members as a spinner; the global bar has no total to
    // advance against until the catalog installs (`attach_db_progress`). Once a
    // DB is present, count the member on phase + global as usual.
    if rt.db.is_some() {
        rt.progress.inc_both(1);
    } else {
        rt.progress.inc_phase(1);
    }
    // Extraction gated due to not selected by filter
    if let Some(db) = rt.db.as_ref() {
        if rt.config.filter.eager_filter && !db.should_extract_canonical_id(fid)? {
            return Ok(())
        }
    }
    // PRECONDITION: either force && no db is true or we saw at least one member.
    let entry_dst = rt.config.paths.extract_cache_dir().join(name);
    let member_total = entry.size();
    rt.pb.set_position(0);
    rt.pb.set_length(member_total);
    let mut prev = 0u64;
    let mut on_bytes = |bytes: u64| -> Result<()> {
        rt.pb.inc(bytes - prev);
        prev = bytes;
        rt.shutdown.check_in_flight()
    };
    // TODO: Subset checks.
    let sp_res = if rt.config.scan.sparsify {
        let mut buf = vec![0u8; rt.config.scan.page_size];
        match sparse_cp::sparse_copy_reader(
            entry,
            &entry_dst,
            member_total,
            &mut buf,
            |bytes, _size: u64, _dur: Duration| on_bytes(bytes)) {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    } else {
        copy_reader_batched(entry, &entry_dst, on_bytes)
    };

    match sp_res {
        Ok(_) => {
            tracing::debug!("extracted cache payload {}", entry_dst.display());
            ()
        },
        // Clean exit on interrupt: no file error recorded, caller saves state.
        Err(Error::Interrupted) => return Err(Error::Interrupted),
        // Process error to recorder and raise or warn + set flag
        Err(Error::FileStat(FileStatError::Io { source, .. })) => {
            record_file_scan_error(recorder, fid, FileStatError::copy_io(&entry_dst, &source));
            if rt.config.process.force {
                tracing::warn!("failed to extract cache payload {}; continuing (--force)",
                    entry_dst.display());
                match rt.db.as_ref() {
                    Some(ldb) => {
                        ldb.set_file_flag(fid, FileFlag::ErrorWhileExtracting, true)?;
                    }
                    None => force_buffer
                        .as_mut()
                        .expect("INVARIANT ERROR: Buffer must be Some(_) iff --force is set")
                        .1
                        .push(fid),
                }
                return Ok(());
            }
            return Err(Error::io(&entry_dst, source));
        }
        // TODO: Subset checks.
        Err(e) => return Err(e),
    };

    if rt.db.is_none() {
        let buf = force_buffer
            .as_mut()
            .expect("INVARIANT ERROR: Buffer must be Some(_) iff --force is set");
        buf.0.push(fid);
    } else {
        let ldb = rt.db.as_ref().expect(OPT_DB_ERROR);
        ldb.set_file_flag(fid, FileFlag::FileExtracted, true)?;
    }
    Ok(())
}

/// Check if the returned state at the end of scanning the archive matches our expectations and add
/// associated errors if something is unexpected.
fn validate_result(scan: &ScanState, resume_db: bool, force: bool) -> Result<()> {
    // Provenance mismatches are corruption, not internal invariants: stop unless
    // `--force` downgrades them to warnings. See the integrity-check convention.
    let provenance_violation = |msg: &str| -> Result<()> {
        if force {
            tracing::warn!("{msg}");
            Ok(())
        } else {
            // TODO Different error.
            Err(Error::Config(msg.to_string()))
        }
    };
    match (
        scan.saw_manifest_db,
        scan.snapshots_ingested > 0,
        scan.from_footer,
        resume_db,
    ) {
        (false, false, false, false) => {
            panic!("INVARIANT ERROR: Loop exited successfully without a database.");
        }
        // INFO: DB present from restart!
        (false, false, false, true) => {
            provenance_violation(
                "resumed extract over a work DB that has no recorded manifest, \
                 snapshot or footer provenance"
            )?;
        }
        (false, false, true, _) => {
            provenance_violation(
                "footer present but the archive never contained a snapshot.sqlite or \
                 manifest.sqlite. Truncated archive?"
            )?;
        }
        (false, true, false, _) => {
            tracing::warn!(
                "{} snapshot(s) ingested, no manifest or footer. Truncated archive?",
                scan.snapshots_ingested
            );
        }
        (false, true, true, _) => {
            tracing::warn!("footer and {} snapshot(s) but no manifest. Truncated Archive? ",
                           scan.snapshots_ingested);
        }
        (true, false, false, _) => {
            tracing::warn!("manifest present, no snapshot ingested. Truncated archive?");
        }
        (true, false, true, _) => {
            provenance_violation(
                "footer present with manifest but no snapshot.sqlite; \
                 archive truncated or corrupt?
                 => a footer should only be appended after a finishing snapshot!"
            )?;
        }
        (true, true, footer, _) => {
            let footer_string = if footer { "footer, " } else { "" };
            tracing::debug!(
                snapshots_ingested = scan.snapshots_ingested,
                "expected scan outcome: {footer_string}manifest and at least one snapshot"
            );
        }
    }
    Ok(())
}

/// Copy the database out of the archive to `dst`, capturing an io failure in the
/// persistent error log (best-effort; a failed record is ignored) before re-raising.
fn captured_extract_database<R: Read> (
    dst: &Path,
    entry: &mut R,
    recorder: &mut Recorder)
    -> Result<()> {
    match fs::File::create(dst) {
        Ok(mut out) => match io::copy(entry, &mut out) {
            Ok(_) => {
                drop(out);
                Ok(())
            }
            Err(e) => {
                drop(out);
                let err = Error::io(dst, e);
                record_session_error(recorder, err.to_file_stat(Some(dst)));
                Err(err)
            }
        },
        Err(e) => {
            let err = Error::io(dst, e);
            record_session_error(recorder, err.to_file_stat(Some(dst)));
            Err(err)
        }
    }
}

/// Open a preexisting database. Thin wrapper that also initializes the runtime state.
fn open_resume_database(db_path: &Path)
    -> Result<Option<Database>> {
    let opened = Database::open(db_path)?;
    opened.init_extract_runtime_state()?;

    Ok(Some(opened))
}

/// Function deals with initial installation of the database (being aware of the footer)
fn install_database(
    rt: &mut ScanRTState,
    db_path: &Path,
    entry: &mut Entry<BufReader<Box<dyn Read>>>,
    recorder: &mut Recorder,
    fb: &mut Option<ParseFilterBuffer>)
    -> Result<()> {

    captured_extract_database(&rt.config.paths.temp_db(), entry, recorder)?;
    match open_initial_database(rt, db_path, fb) {
        Ok(db) => {
            rt.db = Some(db);
            attach_db_progress(rt.db.as_ref().expect(OPT_DB_ERROR), rt.progress, None)?;
            Ok(())
        },
        Err(e) => {
            record_session_error(recorder, e.to_file_stat(Some(db_path)));
            Err(e)
        }
    }
}

/// Install catalog from `temp` into `target`, normalize, and init extract runtime state.
fn open_initial_database(
    rt: &ScanRTState,
    target: &Path,
    fb: &mut Option<ParseFilterBuffer>)
    -> Result<Database> {

    // INFO: Will move the database into the correct place.
    Database::install_initial_manifest(&rt.config.paths.temp_db(), target)?;
    let opened = Database::open(target)?;
    opened.init_extract_runtime_state()?;
    opened.normalize_installed_catalog()?;
    let irt = ExtractRTArgs {
        config: rt.config, db: &opened, shutdown: rt.shutdown, progress: rt.progress
    };
    store_filter_buffer(fb, &irt)?;
    Ok(opened)
}

/// Extract and sanitize the entry basename from a tar member path.
///
/// Member names MUST follow the tar-dedup convention (a content id or a catalog
/// name — both UTF-8); a non-UTF8 name is lossy-converted so that it provably
/// fails `parse_content_id` and hits the `(other, _)` reject arm in `process_entry`.
/// INFO: Explicit result desired, since failure here means user error not internal programming
///     error.
fn entry_name(path: &Path) -> Result<String> {
    let cleaned = path.clean();
    let file = cleaned.file_name();
    let name = match file {
        Some(f) => f.to_string_lossy().into_owned(),
        None => return Err(Error::Config(format!("Invalid tar archive format. Tar member path \
        contained entry without a valid file name. Offending member name: {}", path.display()))),
    };
    debug_assert!(
        !(name.contains('/') || name.contains('\\') || name == ".." || name == "."),
        "INVARIANT BROKEN: Entry Name contained an illegal character."
    );
    Ok(name)
}

//==================================================================================================
// Testing
//==================================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ExtractConfig;
    use crate::db::flags::FileFlag;
    use crate::db::types::{FileId, FilePhase, FileRecord, FileType, StrippedRecord};
    use crate::progress;
    use crate::test_support::{
        add_archived_canonical, append_member, build_archive, build_footer_archive,
        build_multi_catalog, content_id_name, corrupt_byte, insert, seed_filter_reasons,
        write_catalog,
    };
    use crate::unarchive::filter::ingest_filters;
    use progress::EXTRACT_MULTIPLIER;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::PathBuf;

    /// Multi-session counter-example: the last tar member IS a snapshot, but it
    /// is a mid-archive checkpoint that only confirms the first candidate. The
    /// second candidate never appears in any snapshot → stays `staged` while
    /// `last_member_was_snapshot == true`.
    fn archive_with_partial_snapshot(dir: &Path) -> (ExtractConfig, String) {
        let catalog = dir.join("catalog.sqlite");
        let db = Database::open(&catalog).expect("open catalog");
        let c1 = add_archived_canonical(&db, "/one.txt", 7, FilePhase::Staged);
        let c2 = add_archived_canonical(&db, "/two.txt", 8, FilePhase::Staged);
        let member = db.get_file_by_id::<FileRecord>(c1)
            .expect("get").expect("row").tar_member_name().expect("content id");
        // Both payloads ARE in the stream — only the confirmation snapshot is
        // partial (session-1 checkpoint), so the second candidate stays `staged`
        // with its payload extracted.
        let member2 = db.get_file_by_id::<FileRecord>(c2)
            .expect("get").expect("row").tar_member_name().expect("content id");
        db.checkpoint().expect("checkpoint");
        drop(db);

        // The in-stream snapshot only lists `/one.txt` (session-1 checkpoint).
        let snap_path = dir.join("snapshot.sqlite");
        let snap = Database::open(&snap_path).expect("open snapshot");
        let s1 = insert(&snap, "/one.txt", 4, FileType::File);
        seed_filter_reasons(&snap, s1);
        snap.update_file_inspection_per_id(s1, [7u8; 20], 0, false).expect("digest");
        snap.mark_self_canonical(s1).expect("self canonical");
        snap.mark_file_phase(s1, FilePhase::Archived).expect("phase");
        snap.checkpoint().expect("checkpoint");
        drop(snap);

        let catalog_bytes = fs::read(&catalog).expect("read catalog");
        let snap_bytes = fs::read(&snap_path).expect("read snapshot");
        let archive_path = dir.join("archive.tar");
        let mut builder = tar::Builder::new(fs::File::create(&archive_path).expect("create tar"));
        append_member(&mut builder, SNAPSHOT_INIT_TAR_NAME, &catalog_bytes);
        append_member(&mut builder, &member, b"data");
        append_member(&mut builder, &member2, b"data");
        append_member(&mut builder, SNAPSHOT_TAR_NAME, &snap_bytes);
        builder.finish().expect("finish tar");

        let config = ExtractConfig::for_scan_test(archive_path, dir.join("work"), dir.join("out"));
        (config, member)
    }

    /// Head-truncated archive: no manifest, `[payload, snapshot]`. The snapshot body
/// is the catalog, so a DB materializes mid-scan (`--force`). With `sabotage`,
/// the cache copy of the payload fails (destination is pre-created as a dir).
fn archive_head_truncated(dir: &Path, sabotage: bool) -> (ExtractConfig, String) {
        let catalog = dir.join("catalog.sqlite");
        let member = write_catalog(&catalog);
        let catalog_bytes = fs::read(&catalog).expect("read catalog");

        let archive_path = dir.join("archive.tar");
        let mut builder = tar::Builder::new(fs::File::create(&archive_path).expect("create tar"));
        append_member(&mut builder, &member, b"data");
        append_member(&mut builder, SNAPSHOT_TAR_NAME, &catalog_bytes);
        builder.finish().expect("finish tar");

        let config = ExtractConfig::for_scan_test(archive_path, dir.join("work"), dir.join("out"));
        let cache = config.paths.extract_cache_dir();
        fs::create_dir_all(&cache).expect("cache dir");
        if sabotage {
            fs::create_dir(cache.join(&member)).expect("sabotage member dst");
        }
        (config, member)
    }

    #[test]
    fn scan_no_db_success_buffered_then_flagged_on_snapshot() {
        // `--force`, first member is a payload (no DB yet): on success the fid is
        // buffered and gets `FileExtracted` once the snapshot installs a DB.
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, _member) = archive_head_truncated(dir.path(), false);
        let db_path = config.paths.db_path();

        let db = run(&config, &db_path, &Shutdown::detached(),
                     &mut Recorder::speculative(false, None),
                     &mut Some(ParseFilterBuffer::default()),
                     &ProgressBarSet::new(
                         EXTRACT_MULTIPLIER))
            .expect("scan completes under force");

        // Row 1 is the canonical in `write_catalog`.
        let rec = db.get_file_by_id::<StrippedRecord>(FileId(1))
            .expect("get").expect("row");
        assert!(rec.flags.get(FileFlag::FileExtracted));
        assert!(!rec.flags.get(FileFlag::ErrorWhileExtracting));
        // canonical + the duplicate, both unarchived at the end.
        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("count"), 2);
    }

    #[test]
    fn scan_no_db_failure_buffered_then_flagged_on_snapshot() {
        // `--force`, no manifest: the failed payload is buffered and, once the
        // trailing snapshot installs a DB, its `ErrorWhileExtracting` flag is
        // ingested instead of being dropped.
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, _member) = archive_head_truncated(dir.path(), true);
        let db_path = config.paths.db_path();

        let db = run(&config, &db_path, &Shutdown::detached(),
                     &mut Recorder::speculative(true, None),
                     &mut Some(ParseFilterBuffer::default()),
                     &ProgressBarSet::new(
                         EXTRACT_MULTIPLIER))
            .expect("scan completes under force");

        let rec = db.get_file_by_id::<StrippedRecord>(FileId(1))
            .expect("get").expect("row");
        assert!(rec.flags.get(FileFlag::ErrorWhileExtracting),
                "failure flag must survive the no-db window");
        assert!(!rec.flags.get(FileFlag::FileExtracted));
        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("count"), 2);
        assert!(db.count_errors(Some("scan_tar")).expect("errors") >= 1);
    }

    /// Manifest-provenance catalog (two candidates, one present + one whose
    /// payload member is never written) → `manifest + payload + snapshot`.
    /// The `absent` canonical ends the scan `archived` with no flags (bucket C).
    fn archive_with_missing_member(dir: &Path) -> (ExtractConfig, String) {
        let catalog = dir.join("catalog.sqlite");
        let db = Database::open(&catalog).expect("open catalog");
        let present = add_archived_canonical(&db, "/present.txt", 7, FilePhase::Archived);
        add_archived_canonical(&db, "/absent.txt", 8, FilePhase::Archived);
        let member = db.get_file_by_id::<FileRecord>(present)
            .expect("get").expect("row").tar_member_name().expect("content id");
        db.checkpoint().expect("checkpoint");
        drop(db);

        let catalog_bytes = fs::read(&catalog).expect("read catalog");
        let archive_path = dir.join("archive.tar");
        let mut builder = tar::Builder::new(fs::File::create(&archive_path).expect("create tar"));
        append_member(&mut builder, SNAPSHOT_INIT_TAR_NAME, &catalog_bytes);
        append_member(&mut builder, &member, b"data");
        append_member(&mut builder, SNAPSHOT_TAR_NAME, &catalog_bytes);
        builder.finish().expect("finish tar");

        let config = ExtractConfig::for_scan_test(archive_path, dir.join("work"), dir.join("out"));
        (config, member)
    }

    /// Leading manifest + one payload, **no** snapshot — the tail-truncated case.
    /// The candidate stays `staged` (bucket A) since no snapshot confirms it.
    fn archive_without_snapshot(dir: &Path) -> (ExtractConfig, String) {
        let catalog = dir.join("catalog.sqlite");
        let db = Database::open(&catalog).expect("open catalog");
        let canonical = add_archived_canonical(&db, "/canonical.txt", 7, FilePhase::Staged);
        let member = db.get_file_by_id::<FileRecord>(canonical)
            .expect("get").expect("row").tar_member_name().expect("content id");
        db.checkpoint().expect("checkpoint");
        drop(db);

        let catalog_bytes = fs::read(&catalog).expect("read catalog");
        let archive_path = dir.join("archive.tar");
        let mut builder = tar::Builder::new(fs::File::create(&archive_path).expect("create tar"));
        append_member(&mut builder, SNAPSHOT_INIT_TAR_NAME, &catalog_bytes);
        append_member(&mut builder, &member, b"data");
        builder.finish().expect("finish tar");

        let config = ExtractConfig::for_scan_test(archive_path, dir.join("work"), dir.join("out"));
        (config, member)
    }

    #[test]
    fn scan_caches_payloads_and_promotes_on_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, member) = build_archive(dir.path());
        let db_path = config.paths.db_path();

        let db = run(&config, &db_path, &Shutdown::detached(),
                     &mut Recorder::speculative(false, None),
                     &mut Some(ParseFilterBuffer::default()),
                     &ProgressBarSet::new(
                         EXTRACT_MULTIPLIER))
            .expect("scan");

        assert!(config.paths.extract_cache_dir().join(&member).is_file());
        assert_eq!(
            db.count_files_in_phase(FilePhase::Unarchived).expect("count"),
            2
        );
        assert_eq!(db.count_missing_payloads().expect("missing"), 0);
        assert_eq!(db.count_extracted_canonical().expect("canonical"), 1);

        let state = db.load_extract_scan_state().expect("state");
        assert!(state.saw_manifest_db);
        assert!(state.saw_any_members);
        assert_eq!(state.last_member_index, 2);
        assert_eq!(state.snapshots_ingested, 1);
        assert!(!state.from_footer);
    }

    #[test]
    fn scan_force_payload_failure_marks_error_flag_and_promotes() {
        // B2: a failed payload copy under (hardcoded) --force records
        // ErrorWhileExtracting and continues; B1 then blanket-promotes the
        // leftover `archived` rows and records a session error.
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, member) = build_archive(dir.path());
        let db_path = config.paths.db_path();

        // Sabotage the payload destination: a directory at the member path makes
        // the cache copy fail (File::create on an existing dir).
        let cache = config.paths.extract_cache_dir();
        fs::create_dir_all(&cache).expect("cache dir");
        fs::create_dir(cache.join(&member)).expect("sabotage member dst");

        let db = run(&config, &db_path, &Shutdown::detached(),
                     &mut Recorder::speculative(true, None),
                     &mut Some(ParseFilterBuffer::default()),
                     &ProgressBarSet::new(
                         EXTRACT_MULTIPLIER))
            .expect("scan completes under force");

        // Row 1 is the canonical in `write_catalog`: tolerated, not extracted.
        let rec = db.get_file_by_id::<StrippedRecord>(FileId(1))
            .expect("get").expect("row exists");
        assert!(rec.flags.get(FileFlag::ErrorWhileExtracting));
        assert!(!rec.flags.get(FileFlag::FileExtracted));
        // End-of-scan gate sweep promoted the leftover `archived` rows.
        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("count"), 2);
        assert_eq!(db.count_files_in_phase(FilePhase::Archived).expect("count"), 0);
        // The leftover promote is recorded as a session error.
        assert!(db.count_errors(Some("scan_tar")).expect("errors") >= 1);
    }

    #[test]
    fn scan_missing_member_triaged_under_force() {
        // A canonical marked `AppendedPath` whose payload member never appears in
        // the stream (head/middle-truncated or fabricated archive) lands in the
        // "payload not found" bucket and is swept to `unarchived` under --force.
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, _member) = archive_with_missing_member(dir.path());
        let db_path = config.paths.db_path();

        let db = run(&config, &db_path, &Shutdown::detached(),
                     &mut Recorder::speculative(true, None),
                     &mut Some(ParseFilterBuffer::default()),
                     &ProgressBarSet::new(
                         EXTRACT_MULTIPLIER))
            .expect("scan completes under force");

        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("count"), 2);
        assert_eq!(db.count_entries().expect("entries"), 2);
        // The absent canonical was never extracted and carries no error flag.
        let rec = db.get_file_by_id::<StrippedRecord>(FileId(2))
            .expect("get").expect("row");
        assert!(!rec.flags.get(FileFlag::FileExtracted));
        assert!(!rec.flags.get(FileFlag::ErrorWhileExtracting));
        assert!(db.count_errors(Some("scan_tar")).expect("errors") >= 1);
    }

    #[test]
    fn scan_truncated_tail_staged_triaged_under_force() {
        // No final snapshot/footer: the candidate stays `staged` (bucket A).
        // Under --force the gate warns and sweeps it to `unarchived`.
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, _member) = archive_without_snapshot(dir.path());
        let db_path = config.paths.db_path();

        let db = run(&config, &db_path, &Shutdown::detached(),
                     &mut Recorder::speculative(true, None),
                     &mut Some(ParseFilterBuffer::default()),
                     &ProgressBarSet::new(EXTRACT_MULTIPLIER))
            .expect("scan completes under force");

        assert_eq!(db.count_files_in_phase(FilePhase::Staged).expect("staged"), 0);
        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("count"), 1);
        assert!(db.count_errors(Some("scan_tar")).expect("errors") >= 1);
    }

    #[test]
    fn scan_truncated_tail_staged_panics_without_force() {
        // Without --force the same leftover `staged` row aborts the run with an
        // explicit truncation message (not a vague "missing files" one).
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut config, _member) = archive_without_snapshot(dir.path());
        config.process.force = false;
        let db_path = config.paths.db_path();

        let res = catch_unwind(AssertUnwindSafe(|| {
            run(&config, &db_path, &Shutdown::detached(),
                &mut Recorder::speculative(false, None),
                &mut Some(ParseFilterBuffer::default()),
                &ProgressBarSet::new(
                    EXTRACT_MULTIPLIER))
        }));
        let err = res.err().expect("scan must panic without --force");
        let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(msg.contains("never confirmed"), "truncation must be explicit: {msg}");
        assert!(msg.contains("unarchived"), "message names the invariant: {msg}");
    }

    #[cfg(unix)]
    #[test]
    fn entry_name_lossy_converts_invalid_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let os = std::ffi::OsStr::from_bytes(&[0x62, 0xFF, 0xFE]);
        let path = PathBuf::from(&os);
        let name = entry_name(&path).expect("lossy conversion always succeeds");
        assert!(name.contains('\u{FFFD}'));
        // Lossy names are not content ids → the caller's `(other, _)` arm rejects.
        assert!(parse_content_id(&name).is_err());
    }

    #[test]
    fn scan_partial_snapshot_staged_panics_without_force() {
        // Counter-example: `last_member_was_snapshot == true` must NOT prove the
        // archive intact — a mid-archive checkpoint leaves later-session
        // candidates `staged`, and the gate still reports the truncation.
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut config, _member) = archive_with_partial_snapshot(dir.path());
        config.process.force = false;
        let db_path = config.paths.db_path();

        let res = catch_unwind(AssertUnwindSafe(|| {
            run(&config, &db_path, &Shutdown::detached(),
                &mut Recorder::speculative(false, None),
                &mut Some(ParseFilterBuffer::default()),
                &ProgressBarSet::new(
                    EXTRACT_MULTIPLIER))
        }));
        let err = res.err().expect("scan must panic without --force");
        let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(msg.contains("never confirmed"), "truncation must be explicit: {msg}");
        assert!(
            msg.contains("snapshot.sqlite"),
            "message notes the last member was a (possibly mid-archive) snapshot: {msg}"
        );
    }

    #[test]
    fn scan_interrupt_persists_state_and_resume_skips_processed_members() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, _member) = build_archive(dir.path());
        let db_path = config.paths.db_path();

        // First pass installs the catalog so later passes take the resume path.
        run(&config, &db_path, &Shutdown::detached(),
            &mut Recorder::speculative(false, None),
            &mut Some(ParseFilterBuffer::default()),
            &ProgressBarSet::new(EXTRACT_MULTIPLIER))
            .expect("first scan");

        // Interrupt before the first member: state is saved, Interrupted propagates.
        let shutdown = Shutdown::detached();
        shutdown.request_graceful();
        let progress = ProgressBarSet::new(EXTRACT_MULTIPLIER);
        match run(&config, &db_path, &shutdown,
                  &mut Recorder::speculative(false, None),
                  &mut Some(ParseFilterBuffer::default()), &progress) {
            Err(Error::Interrupted) => {}
            Err(other) => panic!("expected Interrupted, got {other:?}"),
            Ok(_) => panic!("expected Interrupted, scan returned Ok"),
        }

        let db = Database::open(&db_path).expect("open");
        let mut state = db.load_extract_scan_state().expect("state");
        assert_eq!(state.last_member_index, 2);

        // Pretend the interrupt happened right after the leading manifest and roll
        // the catalog back, so the resumed pass has to redo members 1 and 2.
        state.last_member_index = 0;
        db.save_extract_scan_state(&state).expect("save");
        db.normalize_installed_catalog().expect("rollback");
        db.checkpoint().expect("checkpoint");
        drop(db);

        // Resuming must skip the manifest member; re-reading it would be an error.
        let db = run(&config, &db_path, &Shutdown::detached(),
                     &mut Recorder::speculative(false, None),
                     &mut Some(ParseFilterBuffer::default()),
                     &ProgressBarSet::new(
                         EXTRACT_MULTIPLIER))
            .expect("resumed scan");
        assert_eq!(
            db.count_files_in_phase(FilePhase::Unarchived).expect("count"),
            2
        );
        let state = db.load_extract_scan_state().expect("state");
        assert_eq!(state.last_member_index, 2);
        // One from the first pass, one from the resumed pass; the interrupt added none.
        assert_eq!(state.snapshots_ingested, 2);
    }

    // ---------------------------------------------------------------------
    // Resume: rewind the saved scan state and re-run.
    // ---------------------------------------------------------------------

    fn run_scan(config: &ExtractConfig, db_path: &Path) -> Database {
        run(config, db_path, &Shutdown::detached(),
            &mut Recorder::speculative(false, None),
            &mut Some(ParseFilterBuffer::default()),
            &ProgressBarSet::new(EXTRACT_MULTIPLIER))
            .expect("scan")
    }

    /// Rewind the saved scan state so the next pass skips members `0..=last_index`
    /// (the resume bound). When `keep_extracted` is false, `FileExtracted` is
    /// cleared (mirroring a scan interrupted before any payload completed), so the
    /// resumed pass re-extracts from scratch; when true, already-extracted rows
    /// keep their flag and only the later members are reprocessed.
    fn rewind_scan_to(db_path: &Path, last_index: u64, keep_extracted: bool) {
        let db = Database::open(db_path).expect("open");
        let mut state = db.load_extract_scan_state().expect("state");
        state.last_member_index = last_index;
        db.save_extract_scan_state(&state).expect("save");
        if !keep_extracted {
            db.normalize_installed_catalog().expect("clear extract flags");
        }
        db.checkpoint().expect("checkpoint");
        drop(db);
    }

    #[test]
    fn scan_resume_zero_extracted_reextracts_everything() {
        // Full run, then rewind to "only the manifest was processed": the resumed
        // pass must re-extract every payload member.
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, _members) = build_multi_catalog(dir.path(), 3);
        let db_path = config.paths.db_path();

        let db = run_scan(&config, &db_path);
        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("c"), 4);
        drop(db);

        rewind_scan_to(&db_path, 0, false);
        let db = run_scan(&config, &db_path);
        assert_eq!(db.count_extracted_canonical().expect("extracted"), 3);
        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("c"), 4);
        let state = db.load_extract_scan_state().expect("state");
        // manifest(0) + 3 payloads(1..=3) + snapshot(4)
        assert_eq!(state.last_member_index, 4);
        assert_eq!(state.snapshots_ingested, 2);
    }

    #[test]
    fn scan_resume_one_extracted_reapplies_trailing_snapshot() {
        // Rewind after the payload member but keep its `FileExtracted` flag: the
        // resumed pass only has to reprocess the trailing snapshot.
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, _member) = build_archive(dir.path());
        let db_path = config.paths.db_path();

        run_scan(&config, &db_path);
        rewind_scan_to(&db_path, 1, true);
        let db = run_scan(&config, &db_path);
        assert_eq!(db.count_extracted_canonical().expect("extracted"), 1);
        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("c"), 2);
        let state = db.load_extract_scan_state().expect("state");
        assert_eq!(state.last_member_index, 2);
        assert_eq!(state.snapshots_ingested, 2);
    }

    #[test]
    fn scan_resume_many_extracted_skips_already_processed_members() {
        // >10 payloads; rewind mid-stream and keep the flags of the already
        // extracted ones. The resumed pass re-processes only the tail members.
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, _members) = build_multi_catalog(dir.path(), 12);
        let db_path = config.paths.db_path();

        run_scan(&config, &db_path);
        // manifest(0) + payloads 1..=6 processed → 6 extracted, tail = 7..=13.
        rewind_scan_to(&db_path, 6, true);
        let db = run_scan(&config, &db_path);
        assert_eq!(db.count_extracted_canonical().expect("extracted"), 12);
        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("c"), 13);
        let state = db.load_extract_scan_state().expect("state");
        assert_eq!(state.last_member_index, 13);
        assert_eq!(state.snapshots_ingested, 2);
    }

    // ---------------------------------------------------------------------
    // Footer + archive integrity.
    // ---------------------------------------------------------------------

    /// The footer's `u64 offset` (LE) lives in the last 8 bytes of the file and
    /// points at the leading `FOOTER_MAGIC` (the start of the trailer).
    fn footer_offset(archive_path: &Path) -> u64 {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = fs::File::open(archive_path).expect("open");
        let len = f.metadata().expect("meta").len();
        f.seek(SeekFrom::Start(len - 8)).expect("seek");
        let mut buf = [0u8; 8];
        f.read_exact(&mut buf).expect("read");
        u64::from_le_bytes(buf)
    }

    #[test]
    fn scan_footer_archive_succeeds_and_records_provenance() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, member) = build_footer_archive(dir.path());
        let db_path = config.paths.db_path();

        let db = run_scan(&config, &db_path);
        assert!(config.paths.extract_cache_dir().join(&member).is_file());
        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("c"), 2);
        let state = db.load_extract_scan_state().expect("state");
        assert!(state.from_footer, "footer provenance must be recorded");
        assert!(
            state.saw_manifest_db,
            "the in-tar manifest member is still walked (no-op install)"
        );
        assert_eq!(state.snapshots_ingested, 1);
    }

    #[test]
    fn scan_footer_content_corruption_aborts_unless_force() {
        // Flip a byte in the archive content region [0, offset): the stored
        // content sha1 no longer matches → verify_archive_integrity returns
        // Ok(false), a Config error without --force. Byte 1024 sits inside the
        // manifest member's data (never opened — the footer wins), so the tar
        // stream itself stays readable.
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut config, _member) = build_footer_archive(dir.path());
        let db_path = config.paths.db_path();
        config.process.force = false;
        corrupt_byte(&config.paths.archive_path, 1024);

        let mut rec = Recorder::speculative(true, None);
        let err = match run(&config, &db_path, &Shutdown::detached(), &mut rec,
                            &mut Some(ParseFilterBuffer::default()),
                            &ProgressBarSet::new(EXTRACT_MULTIPLIER)) {
            Ok(_) => panic!("content corruption must abort without --force"),
            Err(e) => e,
        };
        assert!(matches!(err, Error::Config(_)), "{err:?}");
        assert!(!rec.is_empty(), "integrity mismatch must be recorded");
    }

    #[test]
    fn scan_footer_content_corruption_warns_under_force() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, member) = build_footer_archive(dir.path());
        let db_path = config.paths.db_path();
        corrupt_byte(&config.paths.archive_path, 1024);

        // for_scan_test defaults force=true → warn + continue.
        let db = run_scan(&config, &db_path);
        assert!(config.paths.extract_cache_dir().join(&member).is_file());
        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("c"), 2);
    }

    #[test]
    fn scan_footer_leading_magic_corruption_aborts_regardless_of_force() {
        // Corrupt the leading MAGIC at `offset`. The footer's presence gate
        // (trailing MAGIC + offset) still passes, so locate_footer reads the
        // leading MAGIC, hits a mismatch and returns Err(Config) — which
        // run_inner propagates unconditionally (only Err(FileStat) is downgraded
        // to footer-less handling at load time).
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, _member) = build_footer_archive(dir.path());
        let db_path = config.paths.db_path();
        corrupt_byte(&config.paths.archive_path, footer_offset(&config.paths.archive_path));

        let err = match run(&config, &db_path, &Shutdown::detached(),
                            &mut Recorder::speculative(false, None),
                            &mut Some(ParseFilterBuffer::default()),
                            &ProgressBarSet::new(EXTRACT_MULTIPLIER)) {
            Ok(_) => panic!("corrupt leading magic must abort"),
            Err(e) => e,
        };
        assert!(matches!(err, Error::Config(_)), "{err:?}");
    }

    // ---------------------------------------------------------------------
    // Abort / edge paths: empty archive, payload-only (no catalog), stray cache file.
    // ---------------------------------------------------------------------

    #[test]
    fn scan_empty_archive_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive_path = dir.path().join("empty.tar");
        let mut builder = tar::Builder::new(fs::File::create(&archive_path).expect("create tar"));
        builder.finish().expect("finish empty tar");
        let config = ExtractConfig::for_scan_test(
            archive_path, dir.path().join("work"), dir.path().join("out"));
        let db_path = config.paths.db_path();

        let err = match run(&config, &db_path, &Shutdown::detached(),
                            &mut Recorder::speculative(false, None),
                            &mut Some(ParseFilterBuffer::default()),
                            &ProgressBarSet::new(EXTRACT_MULTIPLIER)) {
            Ok(_) => panic!("empty archive must error"),
            Err(e) => e,
        };
        assert!(matches!(err, Error::Config(_)), "{err:?}");
        assert!(err.to_string().contains("Empty"), "{err}");
    }

    #[test]
    fn scan_payload_only_aborts_and_removes_cache_under_force() {
        // A stream of only a canonical payload (no manifest/snapshot/footer) can
        // never produce a catalog. Under --force the run aborts with a Config
        // error and the cache dir is cleaned up.
        let dir = tempfile::tempdir().expect("tempdir");
        let archive_path = dir.path().join("payload-only.tar");
        let member = content_id_name(FileId(1), [7u8; 20], 4);
        let mut builder = tar::Builder::new(fs::File::create(&archive_path).expect("create tar"));
        append_member(&mut builder, &member, b"data");
        builder.finish().expect("finish tar");
        let config = ExtractConfig::for_scan_test(
            archive_path, dir.path().join("work"), dir.path().join("out"));
        let db_path = config.paths.db_path();

        let cache = config.paths.extract_cache_dir();
        let err = match run(&config, &db_path, &Shutdown::detached(),
                            &mut Recorder::speculative(false, None),
                            &mut Some(ParseFilterBuffer::default()),
                            &ProgressBarSet::new(EXTRACT_MULTIPLIER)) {
            Ok(_) => panic!("payload-only archive must abort"),
            Err(e) => e,
        };
        assert!(matches!(err, Error::Config(_)), "{err:?}");
        assert!(
            !cache.exists(),
            "cache dir must be removed on the no-DB abort"
        );
    }

    #[test]
    fn scan_payload_only_rejected_at_first_member_gate_without_force() {
        // Without --force the first member gate rejects a leading canonical
        // before any extraction; the `rt.db.is_none()` panic path further down
        // is unreachable (no-force forces a leading manifest, which installs a
        // DB), so this documents the reachable behavior.
        let dir = tempfile::tempdir().expect("tempdir");
        let archive_path = dir.path().join("payload-only.tar");
        let member = content_id_name(FileId(1), [7u8; 20], 4);
        let mut builder = tar::Builder::new(fs::File::create(&archive_path).expect("create tar"));
        append_member(&mut builder, &member, b"data");
        builder.finish().expect("finish tar");
        let mut config = ExtractConfig::for_scan_test(
            archive_path, dir.path().join("work"), dir.path().join("out"));
        config.process.force = false;
        let db_path = config.paths.db_path();

        let err = match run(&config, &db_path, &Shutdown::detached(),
                            &mut Recorder::speculative(false, None),
                            &mut Some(ParseFilterBuffer::default()),
                            &ProgressBarSet::new(EXTRACT_MULTIPLIER)) {
            Ok(_) => panic!("leading canonical without force must be rejected"),
            Err(e) => e,
        };
        assert!(matches!(err, Error::Config(_)), "{err:?}");
        assert!(err.to_string().contains("first tar member"), "{err}");
    }

    #[test]
    fn scan_stray_cached_payload_panics_without_force() {
        // check_cached_payloads: a payload-shaped file in the cache dir whose row
        // is not marked FileExtracted is an anomaly. Without --force it panics.
        // The stray names the *duplicate* row (id 2): it exists in the catalog
        // but is never extracted.
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut config, _member) = build_archive(dir.path());
        config.process.force = false;
        let db_path = config.paths.db_path();
        let cache = config.paths.extract_cache_dir();
        fs::create_dir_all(&cache).expect("cache dir");
        fs::write(
            cache.join(content_id_name(FileId(2), [8u8; 20], 4)),
            b"stray",
        ).expect("plant stray");

        let res = catch_unwind(AssertUnwindSafe(|| {
            run(&config, &db_path, &Shutdown::detached(),
                &mut Recorder::speculative(false, None),
                &mut Some(ParseFilterBuffer::default()),
                &ProgressBarSet::new(EXTRACT_MULTIPLIER))
        }));
        let err = res.err().expect("scan must panic on a stray payload");
        let msg = err.downcast_ref::<String>().cloned().unwrap_or_default();
        assert!(msg.contains("not recorded as extracted"), "{msg}");
    }

    #[test]
    fn scan_stray_cached_payload_warns_under_force() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, _member) = build_archive(dir.path());
        let db_path = config.paths.db_path();
        let cache = config.paths.extract_cache_dir();
        fs::create_dir_all(&cache).expect("cache dir");
        fs::write(
            cache.join(content_id_name(FileId(2), [8u8; 20], 4)),
            b"stray",
        ).expect("plant stray");

        let db = run_scan(&config, &db_path);
        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("c"), 2);
    }

    // ---------------------------------------------------------------------
    // Extract filters × eager/lazy gating of the payload members.
    // ---------------------------------------------------------------------

    /// Run the scan with user include/exclude patterns ingested into the filter
    /// buffer (matching the real pipeline: `ingest_filters` → `scan::run`).
    fn run_scan_filtered(
        config: &ExtractConfig,
        db_path: &Path,
        recorder: &mut Recorder,
    ) -> Database {
        let fb = ingest_filters(config, recorder).expect("ingest filters");
        run(config, db_path, &Shutdown::detached(), recorder,
            &mut Some(fb), &ProgressBarSet::new(EXTRACT_MULTIPLIER))
            .expect("scan")
    }

    #[test]
    fn scan_eager_filter_skips_excluded_payload_member() {
        // `/f_0.txt` is excluded by the extract filter; with eager_filter on the
        // scan gate skips its payload member (no FileExtracted) and the
        // end-of-scan bucket-D promotion sweeps it to `unarchived`.
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut config, members) = build_multi_catalog(dir.path(), 2);
        config.filter.eager_filter = true;
        config.filter.exclude_patterns = vec!["/f_0\\.txt".into()];
        let db_path = config.paths.db_path();

        let mut rec = Recorder::speculative(true, None);
        let db = run_scan_filtered(&config, &db_path, &mut rec);
        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("c"), 3);
        assert_eq!(db.count_extracted_canonical().expect("extracted"), 1);

        // f_0 (id 1) is excluded: no FileExtracted, and its member is not cached.
        let f0 = db.get_file_by_id::<StrippedRecord>(FileId(1)).expect("get").expect("row");
        assert!(!f0.flags.get(FileFlag::FileExtracted), "excluded member must be skipped");
        assert!(!config.paths.extract_cache_dir().join(&members[0]).exists());
        // f_1 (id 2) is extracted normally.
        let f1 = db.get_file_by_id::<StrippedRecord>(FileId(2)).expect("get").expect("row");
        assert!(f1.flags.get(FileFlag::FileExtracted));
        assert!(config.paths.extract_cache_dir().join(&members[1]).is_file());
        // The eager filter ran and persisted its bookkeeping.
        assert!(db.get_extract_filters_applied().expect("get"));
    }

    #[test]
    fn scan_eager_no_user_filters_takes_fast_path() {
        // `eager_filter` on but no user patterns: the internal `.*` catch-all
        // triggers the apply_no_filter fast path, so the eager call shows no
        // filter sub-bar and every payload is still extracted.
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut config, members) = build_multi_catalog(dir.path(), 2);
        config.filter.eager_filter = true;
        let db_path = config.paths.db_path();

        let db = run_scan(&config, &db_path);
        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("c"), 3);
        assert_eq!(db.count_extracted_canonical().expect("extracted"), 2);
        assert!(config.paths.extract_cache_dir().join(&members[0]).is_file());
        assert!(config.paths.extract_cache_dir().join(&members[1]).is_file());
        assert!(db.get_extract_filters_applied().expect("get"));
    }

    #[test]
    fn scan_lazy_filter_extracts_all_payloads() {
        // Same patterns but eager_filter off: the scan gate does not consult the
        // extract filter, so every payload member is extracted; the filter only
        // takes effect in the later place phase.
        let dir = tempfile::tempdir().expect("tempdir");
        let (mut config, members) = build_multi_catalog(dir.path(), 2);
        config.filter.eager_filter = false;
        config.filter.exclude_patterns = vec!["/f_0\\.txt".into()];
        let db_path = config.paths.db_path();

        let mut rec = Recorder::speculative(true, None);
        let db = run_scan_filtered(&config, &db_path, &mut rec);
        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("c"), 3);
        assert_eq!(db.count_extracted_canonical().expect("extracted"), 2);
        assert!(config.paths.extract_cache_dir().join(&members[0]).is_file());
        assert!(config.paths.extract_cache_dir().join(&members[1]).is_file());
    }

    // ---------------------------------------------------------------------
    // process_entry member-ordering rules.
    // ---------------------------------------------------------------------

    #[test]
    fn scan_manifest_not_first_rejected() {
        // Spec violation: a manifest after any member is unconditionally rejected
        // (the initial db MUST be the first element of the stream).
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog = dir.path().join("catalog.sqlite");
        let member = write_catalog(&catalog);
        let catalog_bytes = fs::read(&catalog).expect("read catalog");
        let archive_path = dir.path().join("archive.tar");
        let mut builder = tar::Builder::new(fs::File::create(&archive_path).expect("create tar"));
        append_member(&mut builder, &member, b"data"); // payload first
        append_member(&mut builder, SNAPSHOT_INIT_TAR_NAME, &catalog_bytes); // manifest second
        builder.finish().expect("finish tar");
        let config = ExtractConfig::for_scan_test(
            archive_path, dir.path().join("work"), dir.path().join("out"));
        let db_path = config.paths.db_path();

        let err = match run(&config, &db_path, &Shutdown::detached(),
                            &mut Recorder::speculative(false, None),
                            &mut Some(ParseFilterBuffer::default()),
                            &ProgressBarSet::new(EXTRACT_MULTIPLIER)) {
            Ok(_) => panic!("manifest not first must be rejected"),
            Err(e) => e,
        };
        assert!(matches!(err, Error::Config(_)), "{err:?}");
        assert!(err.to_string().contains("not at the beginning"), "{err}");
    }

    #[test]
    fn scan_snapshot_first_rejected_without_force() {
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog = dir.path().join("catalog.sqlite");
        let member = write_catalog(&catalog);
        let catalog_bytes = fs::read(&catalog).expect("read catalog");
        let archive_path = dir.path().join("archive.tar");
        let mut builder = tar::Builder::new(fs::File::create(&archive_path).expect("create tar"));
        append_member(&mut builder, SNAPSHOT_TAR_NAME, &catalog_bytes);
        append_member(&mut builder, &member, b"data");
        builder.finish().expect("finish tar");
        let mut config = ExtractConfig::for_scan_test(
            archive_path, dir.path().join("work"), dir.path().join("out"));
        config.process.force = false;
        let db_path = config.paths.db_path();

        let err = match run(&config, &db_path, &Shutdown::detached(),
                            &mut Recorder::speculative(false, None),
                            &mut Some(ParseFilterBuffer::default()),
                            &ProgressBarSet::new(EXTRACT_MULTIPLIER)) {
            Ok(_) => panic!("snapshot-first must be rejected without --force"),
            Err(e) => e,
        };
        assert!(matches!(err, Error::Config(_)), "{err:?}");
        assert!(err.to_string().contains("first tar member is snapshot"), "{err}");
    }

    #[test]
    fn scan_snapshot_first_installs_catalog_under_force() {
        // Head-truncated archive under --force: the snapshot body is treated as
        // the catalog, so the payload member is extracted and the end-of-scan
        // promote lands everything in `unarchived`.
        let dir = tempfile::tempdir().expect("tempdir");
        let catalog = dir.path().join("catalog.sqlite");
        let member = write_catalog(&catalog);
        let catalog_bytes = fs::read(&catalog).expect("read catalog");
        let archive_path = dir.path().join("archive.tar");
        let mut builder = tar::Builder::new(fs::File::create(&archive_path).expect("create tar"));
        append_member(&mut builder, SNAPSHOT_TAR_NAME, &catalog_bytes);
        append_member(&mut builder, &member, b"data");
        builder.finish().expect("finish tar");
        let config = ExtractConfig::for_scan_test(
            archive_path, dir.path().join("work"), dir.path().join("out"));
        let db_path = config.paths.db_path();

        let db = run_scan(&config, &db_path);
        assert_eq!(db.count_files_in_phase(FilePhase::Unarchived).expect("c"), 2);
        assert!(config.paths.extract_cache_dir().join(&member).is_file());
    }

    // ---------------------------------------------------------------------
    // extract_canonical_member: sparse-vs-regular copy, mid-copy interrupt.
    // ---------------------------------------------------------------------

    /// One canonical with a `size`-byte all-zero payload: `manifest + payload +
    /// snapshot`, returned as the config + member name. The row's DB size stays 4
    /// (unused by extraction — the tar member's own `size()` drives the copy).
    fn archive_with_large_payload(dir: &Path, size: usize) -> (ExtractConfig, String) {
        let catalog = dir.join("catalog.sqlite");
        let db = Database::open(&catalog).expect("open catalog");
        let id = add_archived_canonical(&db, "/big.txt", 9, FilePhase::Archived);
        let member = db
            .get_file_by_id::<FileRecord>(id)
            .expect("get")
            .expect("row")
            .tar_member_name()
            .expect("content id");
        db.checkpoint().expect("checkpoint");
        drop(db);

        let catalog_bytes = fs::read(&catalog).expect("read catalog");
        let archive_path = dir.join("archive.tar");
        let mut builder = tar::Builder::new(fs::File::create(&archive_path).expect("create tar"));
        append_member(&mut builder, SNAPSHOT_INIT_TAR_NAME, &catalog_bytes);
        let payload = vec![0u8; size];
        append_member(&mut builder, &member, &payload);
        append_member(&mut builder, SNAPSHOT_TAR_NAME, &catalog_bytes);
        builder.finish().expect("finish tar");

        let config = ExtractConfig::for_scan_test(
            archive_path, dir.join("work"), dir.join("out"));
        (config, member)
    }

    #[cfg(unix)]
    #[test]
    fn scan_sparse_vs_regular_copy_same_content() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let (config0, member) = archive_with_large_payload(dir.path(), 8 * 1024 * 1024);

        // Regular copy (default: sparsify off).
        let mut config = config0.clone();
        config.scan.sparsify = false;
        let db_path = config.paths.db_path();
        let db = run_scan(&config, &db_path);
        drop(db);
        let regular = config.paths.extract_cache_dir().join(&member);
        assert_eq!(fs::metadata(&regular).expect("meta").len(), 8 * 1024 * 1024);

        // Sparse copy into a fresh work dir.
        let mut config = config0;
        config.paths.work_dir = dir.path().join("work_sparse");
        config.scan.sparsify = true;
        let db_path = config.paths.db_path();
        let db = run_scan(&config, &db_path);
        drop(db);
        let sparse = config.paths.extract_cache_dir().join(&member);
        assert_eq!(fs::metadata(&sparse).expect("meta").len(), 8 * 1024 * 1024);

        // The content must be byte-identical regardless of copy strategy.
        assert_eq!(
            fs::read(&regular).expect("read regular"),
            fs::read(&sparse).expect("read sparse"),
        );

        // Allocated blocks: the sparse copy must never allocate more than the
        // regular one. Whether it allocates *less* depends on the filesystem
        // transparently sparsifying zero writes (the caveat in plans/046): only
        // when the regular copy stays fully allocated is a strict reduction
        // required.
        let regular_blocks = fs::metadata(&regular).expect("meta").blocks();
        let sparse_blocks = fs::metadata(&sparse).expect("meta").blocks();
        assert!(sparse_blocks <= regular_blocks, "sparse must not allocate more");
        if regular_blocks * 512 >= 8 * 1024 * 1024 / 2 {
            assert!(
                sparse_blocks < regular_blocks,
                "sparse copy must punch holes when the fs does not sparsify zero writes"
            );
        }
    }

    #[test]
    fn scan_force_interrupt_mid_copy_returns_interrupted() {
        // A large payload gives the copy enough time for a force request to land
        // in-flight. `request_force` makes `check_in_flight` raise Interrupted;
        // a graceful request would NOT (it only stops between members), so the
        // force path is the one that exercises the mid-copy abort.
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, _member) = archive_with_large_payload(dir.path(), 64 * 1024 * 1024);
        let db_path = config.paths.db_path();

        let shutdown = Shutdown::detached();
        let armed = shutdown.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(5));
            armed.request_force();
        });

        let progress = ProgressBarSet::new(EXTRACT_MULTIPLIER);
        match run(&config, &db_path, &shutdown,
                  &mut Recorder::speculative(false, None),
                  &mut Some(ParseFilterBuffer::default()), &progress) {
            Err(Error::Interrupted) => {}
            Err(other) => panic!("expected Interrupted, got {other:?}"),
            Ok(_) => panic!("expected Interrupted, scan returned Ok"),
        }
    }

    // Pure-fn battery: validate_result / entry_name / captured_extract_database.
    // ---------------------------------------------------------------------

    fn scan_state(saw_manifest_db: bool, snapshots: u32, from_footer: bool) -> ScanState {
        ScanState {
            saw_manifest_db,
            snapshots_ingested: snapshots,
            from_footer,
            ..ScanState::default()
        }
    }

    #[test]
    fn validate_result_no_provenance_panics() {
        // (false, false, false, false): the loop "succeeded" without any DB —
        // an internal invariant, so a panic (never a force-able Config error).
        let res = catch_unwind(AssertUnwindSafe(|| {
            validate_result(&scan_state(false, 0, false), false, false)
        }));
        assert!(res.is_err(), "no provenance + no resume DB must panic");
    }

    #[test]
    fn validate_result_provenance_violations_config_or_warn() {
        // Provenance mismatches are corruption: Config error without --force,
        // warn + Ok under --force.
        for (saw_manifest_db, snapshots, from_footer, resume_db, label) in [
            (false, 0, false, true, "resumed work DB without provenance"),
            (false, 0, true, false, "footer present, no snapshot"),
            (true, 0, true, false, "footer + manifest, no snapshot"),
        ] {
            let state = scan_state(saw_manifest_db, snapshots, from_footer);
            let err = validate_result(&state, resume_db, false)
                .expect_err(&format!("{label}: no force must Err"));
            assert!(matches!(err, Error::Config(_)), "{label}: {err:?}");
            validate_result(&state, resume_db, true)
                .expect(&format!("{label}: force must warn + Ok"));
        }
    }

    #[test]
    fn validate_result_benign_combinations_ok() {
        // Truncation signals that only downgrade to a warning (no Config error).
        validate_result(&scan_state(false, 1, false), false, false).expect("snapshot only");
        validate_result(&scan_state(false, 1, true), false, false).expect("footer + snapshot");
        validate_result(&scan_state(true, 0, false), false, false).expect("manifest only");
        validate_result(&scan_state(true, 1, false), false, false).expect("manifest + snapshot");
        validate_result(&scan_state(true, 1, true), false, false).expect("manifest + snapshot + footer");
    }

    #[test]
    fn entry_name_ok_and_missing_file_name() {
        assert_eq!(entry_name(Path::new("abc.txt")).expect("name"), "abc.txt");
        let err = entry_name(Path::new("/")).expect_err("no file name on root");
        assert!(matches!(err, Error::Config(_)), "{err:?}");
        // `Path::file_name()` is None for a trailing ".." too, so the same
        // Config error arm is reached.
        let err = entry_name(Path::new("..")).expect_err("no file name on '..'");
        assert!(matches!(err, Error::Config(_)), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn entry_name_debug_assert_on_backslash_filename() {
        // On Unix `\` is a literal filename char (not a separator), so a tar
        // member name like `a\b` survives clean()/file_name() intact and trips
        // the `contains('\\')` debug_assert. Only in debug builds; tests run in
        // the dev profile.
        let res = catch_unwind(AssertUnwindSafe(|| entry_name(Path::new("a\\b"))));
        assert!(res.is_err(), "backslash filename must trip the debug_assert");
    }

    /// Reader that fails after `remaining` bytes — forces the `io::copy` error arm.
    struct FailingReader {
        remaining: usize,
    }
    impl Read for FailingReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(io::ErrorKind::Other, "boom"));
            }
            let n = buf.len().min(self.remaining);
            buf[..n].fill(0);
            self.remaining -= n;
            Ok(n)
        }
    }

    #[test]
    fn captured_extract_database_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dst = dir.path().join("out.sqlite");
        let mut rec = Recorder::speculative(true, None);
        let mut reader = &b"hello"[..];
        captured_extract_database(&dst, &mut reader, &mut rec).expect("copy");
        assert_eq!(fs::read(&dst).expect("read"), b"hello");
        assert!(rec.is_empty(), "no error recorded on success");
    }

    #[test]
    fn captured_extract_database_copy_error_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dst = dir.path().join("out.sqlite");
        let mut rec = Recorder::speculative(true, None);
        let mut reader = FailingReader { remaining: 3 };
        let err = captured_extract_database(&dst, &mut reader, &mut rec)
            .expect_err("copy must fail");
        assert!(matches!(err, Error::FileStat(_)), "{err:?}");
        assert!(!rec.is_empty(), "session error must be recorded");
    }

    #[test]
    fn captured_extract_database_create_error_records() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dst = dir.path(); // existing directory → File::create fails
        let mut rec = Recorder::speculative(true, None);
        let mut reader = &b"hello"[..];
        let err = captured_extract_database(&dst, &mut reader, &mut rec)
            .expect_err("create must fail");
        assert!(matches!(err, Error::FileStat(_)), "{err:?}");
        assert!(!rec.is_empty(), "session error must be recorded");
    }
}
