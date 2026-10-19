//! Scan/untar: footer-first catalog, else leading `manifest.sqlite`; cache payloads.

use crate::archive_footer::read_footer;
use crate::common::defaults::{SNAPSHOT_INIT_TAR_NAME, SNAPSHOT_TAR_NAME};
use crate::common::files::copy_reader_batched;
use crate::config::ExtractConfig;
use crate::db::content_id::parse_content_id;
use crate::db::flags::{ErrorFlags, FileFlag};
use crate::db::types::{FileId, FilePhase};
use crate::db::{Database, ErrorPhase, ExtractScanState, Recorder};
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
fn attach_db_progress(db: &Database, progress: &ProgressBarSet) -> Result<()> {
    let total = db.count_entries()?;
    let encoded = db.count_encoded_payloads()?;
    progress.set_table_size(total);
    progress.set_phase_kind(BarKind::Count);
    progress.set_phase_total(encoded);
    progress.inc_global(total.saturating_sub(encoded));
    progress.inc_both(db.count_extracted_canonical()?);
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
        filter_run(rt, false)?
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

    // INFO: Noop if dir exists!
    // INFO: If we can't create an extract stage, we can only abort.
    fs::create_dir_all(rt.config.paths.extract_cache_dir())
        .map_err(|e| Error::io(&rt.config.paths.extract_cache_dir(), e))?;

    let resume_db = db_path.is_file();
    // Only a first pass installs the footer catalog; later passes inherit the fact
    // that it came from a footer through `ExtractScanState::from_footer`.
    // INFO: read into a temp file first; installed below via rename to `db_path`.
    let opt_db = read_footer(&rt.config.paths.archive_path, &rt.config.paths.temp_db());
    let footer_this_pass = !resume_db && opt_db.is_ok();

    rt.db = if resume_db {
        open_resume_database(&db_path)?
    } else if footer_this_pass {
        Some(open_initial_database(&rt, db_path, filter_buffer)?)
    } else {
        None
    };

    if let Some(edb) = rt.db.as_ref() {
        rt.progress.set_phase_total(edb.count_entries()?);
        // The scan recorder's auto-flush limit comes from the extract config that
        // governs this run (applied once a fresh-from-disk DB is in hand).
        recorder.set_flush_limit(rt.config.process.auto_flush_limit);
    }

    let mut force_buffer: Option<(Vec<FileId>, Vec<FileId>)> = if rt.config.process.force {
        Some((Vec::new(), Vec::new()))
    } else {
        None
    };

    // Single source of truth for everything the scan knows about the archive.
    // `footer_this_pass` is folded in here and never consulted again.
    let mut scan = match rt.db {
        Some(ref d) => d.load_extract_scan_state()?,
        None => ExtractScanState::default(),
    };
    scan.from_footer |= footer_this_pass;
    scan.scan_complete = false; // Set when archive iterator is empty.

    let corpus = match rt.db {
        Some(ref d) => d.count_entries()?,
        None => 0u64,
    };
    if resume_db {
        tracing::info!(
            "starting scan: total {}, processing {}, pending {} \
             (resume from member {})",
            corpus, corpus, corpus,
            scan.last_member_index.saturating_add(1),
        );
    } else {
        tracing::info!(
            "starting scan: total {}, processing {}, pending {}",
            corpus, corpus, corpus,
        );
    }

    let mut archive = open_tar_archive(
        &rt.config.paths.archive_path, &rt.config.decompression
    )?;

    // Resume skip bound: members up to **and including** the last one fully
    // processed by an earlier pass are skipped (re-ingesting `manifest.sqlite`
    // would be rejected as "not at the beginning"). `saw_any_members` as loaded
    // (not the live flag) distinguishes a fresh db (`last == 0`, nothing
    // processed) from a resume that already consumed the leading manifest
    // (`last == 0`, a member was seen) — both store `last_member_index == 0`.
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

        rt.progress.inc_both(1);
        scan.saw_any_members = true;
        scan.last_member_index = member_index;
        scan.last_member_was_snapshot = name == SNAPSHOT_TAR_NAME;
    }

    if !scan.nt_stopped {
        scan.scan_complete = true;
    }

    // Persist observations before any early return / interrupt propagation.
    store_progress_in_db(&rt, &scan)?;

    if scan.nt_stopped {
        // Best-effort persist whatever was buffered against the pass's database.
        flush_scan_recorder(recorder, rt.db.as_ref())?;
        return Err(Error::Interrupted);
    }

    if !scan.saw_any_members {
        flush_scan_recorder(recorder, rt.db.as_ref())?;
        return Err(Error::Config("Archive is Empty".to_string()));
    }

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
    // PRECONDITION: Archive contained at least one element and at least a database and we
    //   fully consumed the archive.
    validate_result(&scan, resume_db)?;

    let sdb = rt.db.expect(OPT_DB_ERROR);
    let trust_catalog = scan.from_footer || scan.saw_manifest_db || rt.config.process.force;

    // Check for any payloads that were extracted but somehow not recorded
    let (present, new) = sdb.check_cached_payloads(&rt.config.paths.extract_cache_dir())?;
    if new > 0 {
        let msg = format!("Found {new} archive payload(s) which were not recorded as extracted.");
        if rt.config.process.force {
            tracing::warn!("{msg}");
        } else {
            panic!("{msg}");
        }
    }

    // TODO we cannot assert that we have all entries in the files table in 'archived' no?
    report_scan_completeness(&sdb, scan.from_footer, rt.config.process.force)?;
    if trust_catalog {
        let n = sdb.promote_extracted_to_unarchived()?;
        if n > 0 {
            tracing::warn!(
                "promoted {n} extracted row(s) to unarchived without snapshot confirmation \
                 (footer and/or --force)"
            );
        }
    }

    // End-of-scan gate: every row must be `unarchived`, mirroring the archive
    // pipeline's phase-end convention. The intentional extract-filter-excluded
    // bucket (D) is promoted out first; the remaining rows (A/B/C) are real
    // violations and are triaged with a precise breakdown (see
    // `plans/024-scan-end-unarchived.md`).
    let tally = sdb.tally_scan_remainders(rt.config.filter.eager_filter)?;
    if tally.filter_excluded > 0 {
        let promoted = sdb.promote_filter_excluded_remainders()?;
        debug_assert_eq!(promoted, tally.filter_excluded);
        tracing::info!(
            count = tally.filter_excluded,
            "scan: {} file(s) excluded by the extract filter (not extracted by design)",
            tally.filter_excluded
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
             found in the archive stream\n\
             Run with --force to proceed (affected files will be skipped).",
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
            panic!("{msg}");
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

    let _ = fs::remove_file(rt.config.paths.temp_db());
    let _ = fs::remove_file(rt.config.paths.temp_snapshot());
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
    scan: &mut ExtractScanState,
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
        }
        (SNAPSHOT_TAR_NAME, true) => {
            if rt.config.process.force && rt.db.is_none() {
                install_db(rt)?;
                let ref_db = rt.db.as_ref().expect(OPT_DB_ERROR);
                scan.snapshots_ingested = ref_db.record_snapshot_ingested()?;

                if let Some(buf) = force_buffer.as_mut() {
                    for fid in buf.0.drain(..) {
                        ref_db.set_file_flag(fid, FileFlag::FileExtracted, true)?;
                    }
                    for fid in buf.1.drain(..) {
                        ref_db.set_file_flag(fid, FileFlag::ErrorWhileExtracting, true)?;
                    }
                }
                // Confirm from this snapshot after marking buffered extracts.
                ref_db.apply_snapshot_promote_unarchived(&rt.config.paths.temp_db())?;
                // Ingest any scan errors buffered while no DB existed (recorded
                // during the no-DB window) now that one has materialized.
                flush_scan_recorder(recorder, Some(&ref_db))?;
            } else {
                assert!(rt.config.process.force || rt.db.is_some(),
                        "INVARIANT ERROR: without force, the first member of an archive MUST be an \
                        initial db");
                let ldb = rt.db.as_ref().expect(OPT_DB_ERROR);
                let _ = captured_extract_database(&rt.config.paths.temp_snapshot(), entry, recorder)?;
                ldb.apply_snapshot_promote_unarchived(&rt.config.paths.temp_snapshot())?;
                scan.snapshots_ingested = ldb.record_snapshot_ingested()?;
            }
        }
        (cid, saw_first) if let Ok((_, _, fid, _)) = parse_content_id(cid) => {
            if !rt.config.process.force && !saw_first {
                return Err(Error::Config(format!(
                    "first tar member is canonical file {cid} not manifest.sqlite; \
                     bypass available with --force"
                )));
            }
            // Extraction gated due to not selected by filter
            if let Some(db) = rt.db.as_ref() {
                if rt.config.filter.eager_filter && !db.should_extract_canonical_id(fid)? {
                    return Ok(())
                }
            }
            // PRECONDITION: either force && no db is true or we saw at least one member.
            let entry_dst = rt.config.paths.extract_cache_dir().join(name);
            // Stepped member write: sparse copy preserves holes when requested
            // (hole-detection block = `--page-size`), plain batched copy otherwise;
            // both report per-member byte progress on the shared sub-bar.
            let member_total = entry.size();
            rt.pb.set_position(0);
            rt.pb.set_length(member_total);
            let mut prev = 0u64;
            let mut on_bytes = |bytes: u64| -> Result<()> {
                rt.pb.inc(bytes - prev);
                prev = bytes;
                rt.shutdown.check_in_flight()
            };
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
                // Re-attach the destination path (copy helpers lose it) and record.
                Err(Error::FileStat(FileStatError::Io { source, .. })) => {
                    record_file_scan_error(recorder, fid, FileStatError::copy_io(&entry_dst, &source));
                    // B2: under `--force` a failed payload is tolerated — mark the
                    // row `ErrorWhileExtracting` and move on; the missing-payload
                    // report stays visible in the completeness pass.
                    if rt.config.process.force {
                        tracing::warn!(
                            "failed to extract cache payload {}; continuing (--force)",
                            entry_dst.display()
                        );
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
                Err(e) => return Err(e),
            };

            if rt.config.process.force && rt.db.is_none() {
                let buf = force_buffer
                    .as_mut()
                    .expect("INVARIANT ERROR: Buffer must be Some(_) iff --force is set");
                buf.0.push(fid);
            } else {
                let ldb = rt.db.as_ref().expect(OPT_DB_ERROR);
                ldb.set_file_flag(fid, FileFlag::FileExtracted, true)?;
            }
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

/// Check if the returned state at the end of scanning the archive matches our expectations and add
/// associated errors if something is unexpected.
fn validate_result(scan: &ExtractScanState, resume_db: bool) -> Result<()> {
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
            // TODO Technically impossible.
            tracing::warn!(
                "resumed extract over a work DB that has no recorded manifest, \
                 snapshot or footer provenance"
            );
        }
        (false, false, true, _) => {
            tracing::warn!(
                "footer present but the archive never contained a snapshot.sqlite. \
                 Truncated archive?"
            );
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
            tracing::warn!(
                "footer present with manifest but no snapshot.sqlite; \
                 archive truncated or corrupt? \
                 => a footer should only be appended after a finishing snapshot!"
            );
        }
        (true, true, footer, _) => {
            let footer_string = if footer { "footer," } else { "" };
            tracing::debug!(
                snapshots_ingested = scan.snapshots_ingested,
                "expected scan outcome: {footer_string} manifest and at least one snapshot"
            );
        }
    }
    Ok(())
}

fn report_scan_completeness(db: &Database, from_footer: bool, force: bool) -> Result<()> {
    // INFO: AppendedPath = true, FileExtracted = false
    let missing = db.count_missing_payloads()?;
    if missing > 0 {
        let msg = format!(
            "{missing} canonical file(s) have AppendedPath but were not extracted from the archive"
        );
        if force { tracing::warn!("{msg}"); } else { return Err(Error::Config(msg)); }
    }
    // INFO: rows FileExtracted = True || parent i.e. id = canonical_id has FileExtracted = True
    let unconfirmed = db.count_unconfirmed_extracted()?;
    let confirmed = db.count_files_in_phase(FilePhase::Unarchived)?;
    // TODO Sanity Check confirmed I.e. no extracted where
    if unconfirmed > 0 {
        if from_footer {
            tracing::warn!(
                "{unconfirmed} extracted file(s) were not promoted to unarchived; \
                 footer catalog was trusted for salvage"
            );
        } else if confirmed > 0 {
            tracing::warn!(
                "{confirmed} path(s) were confirmed by a snapshot but {unconfirmed} \
                 extracted file(s) were not; the archive tail is incomplete"
            );
        } else {
            tracing::warn!(
                "{unconfirmed} extracted file(s) were not promoted to unarchived and \
                 no snapshot confirmed any path; the archive is most likely incomplete"
            );
        }
    }
    // INFO: ONLY canonical_id = id, FileExtracted = true
    let canonical = db.count_extracted_canonical()?;
    // INFO: Select FileExtracted = true or parent (where canonical_id = id)
    let paths = db.count_extracted_paths()?;
    tracing::info!(
        canonical,
        paths,
        "extract: extracted payloads — {canonical} canonical, {paths} path(s) in groups"
    );

    for (ftype, count) in db.count_non_appended_by_ftype()? {
        tracing::info!(count, "extract: non-AppendedPath entries ftype={ftype}");
    }

    Ok(())
}

/// Store progress in db
fn store_progress_in_db(rt: &ScanRTState, scan: &ExtractScanState) -> Result<()> {
    if let Some(d) = rt.db.as_ref() {
        let mut persisted = d.load_extract_scan_state()?;
        persisted.saw_manifest_db |= scan.saw_manifest_db;
        persisted.saw_any_members |= scan.saw_any_members;
        persisted.scan_complete = scan.scan_complete;
        // `None` sorts below every `Some`, so this keeps the furthest point reached.
        persisted.last_member_index = persisted.last_member_index.max(scan.last_member_index);
        persisted.from_footer |= scan.from_footer;
        persisted.last_member_was_snapshot = scan.last_member_was_snapshot;
        // snapshots_ingested already updated via record_snapshot_ingested.
        d.save_extract_scan_state(&persisted)?;
        d.checkpoint()?;
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
        Ok(opened) => {
            rt.db = Some(opened);
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
    let rt = ExtractRTArgs {
        config: rt.config, db: &opened, shutdown: rt.shutdown, progress: rt.progress
    };
    store_filter_buffer(fb, &rt)?;
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
        add_archived_canonical, append_member, build_archive, insert, seed_filter_reasons,
        write_catalog,
    };
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
        assert!(state.scan_complete);
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
        assert!(!state.scan_complete);
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
        assert!(state.scan_complete);
        assert_eq!(state.last_member_index, 2);
        // One from the first pass, one from the resumed pass; the interrupt added none.
        assert_eq!(state.snapshots_ingested, 2);
    }
}
