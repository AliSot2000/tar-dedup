//! Scan/untar: footer-first catalog, else leading `manifest.sqlite`; cache payloads.

use std::fs;
use std::io::{BufReader, Read, copy};
use std::path::{Path, PathBuf};

use crate::archive_footer::read_footer;
use crate::config::Config;
use crate::db::content_id::parse_content_id;
use crate::db::types::{FileId, FilePhase};
use crate::db::{Database, ExtractScanState};
use crate::error::{Error, Result};
use crate::shutdown::Shutdown;
use crate::tar_reader::open_tar_archive;
use path_clean::PathClean;
use tar::Entry;

const SNAPSHOT_INIT_TAR_NAME: &str = "manifest.sqlite";
const SNAPSHOT_TAR_NAME: &str = "snapshot.sqlite";

const OPT_DB_ERROR: &str = "INVARIANT ERROR: Database expected to be present at this point";

// TODO Fail fast option for - what if extract fails.
// TODO progress bar / spinner when we have a long decompress skip on resume.

// INFO: State Model for a File and its associated canonical row in the db:
//    0. File seen in the stream
//    1. File finished extracting successfully => FileExtracted = True
//    2. Snapshot Encountered => All files with phase='archived' in snapshot && FileExtracted => phase='unarchived'
//       <Report File Counts>
//    4. Promote all FileExtracted = True && phase= 'sparsified' | 'archived' => phase='unarchived'
//    5. Promote remaining entries in DB to 'unarchived'

/// Walk the tar stream: load catalog (footer or leading manifest), cache payloads, promote.
pub fn run(config: &Config, db_path: &Path, shutdown: &Shutdown) -> Result<Database> {
    fs::create_dir_all(config.extract_cache_dir())
        .map_err(|e| Error::io(&config.extract_cache_dir(), e))?;

    let resume_db = db_path.is_file();
    // Only a first pass installs the footer catalog; later passes inherit the fact
    // that it came from a footer through `ExtractScanState::from_footer`.
    let opt_db = read_footer(&config.temp_db(), db_path);
    let footer_this_pass = !resume_db && opt_db.is_ok();

    let mut db = if resume_db {
        let opened = Database::open(db_path)?;
        opened.init_extract_runtime_state()?;
        remove_temp_db(&config.temp_db());
        Some(opened)
    } else if footer_this_pass {
        fs::rename(config.temp_db(), db_path).map_err(|e| Error::io(db_path, e))?;
        let opened = Database::open(db_path)?;
        opened.init_extract_runtime_state()?;
        opened.normalize_installed_catalog()?;
        Some(opened)
    } else {
        remove_temp_db(&config.temp_db());
        None
    };

    let local_dst = config.extract_cache_dir();
    let snapshot_tmp = config.work_dir.join(".snapshot-ingest.tmp");

    let mut force_buffer: Option<Vec<FileId>> =
        if config.force_scan { Some(Vec::new()) } else { None };

    // Single source of truth for everything the scan knows about the archive.
    // `footer_this_pass` is folded in here and never consulted again.
    let mut scan = match db {
        Some(ref d) => d.load_extract_scan_state()?,
        None => ExtractScanState::default(),
    };
    scan.from_footer |= footer_this_pass;
    // Completion describes *this* pass; only exhausting the iterator sets it.
    scan.scan_complete = false;

    // Snapshots seen in this pass are derived from the cumulative total, so there is
    // no second counter that can drift away from the database.
    let snapshots_before = scan.snapshots_ingested;
    let should_skip = resume_db && scan.last_member_index.is_some();
    let resume_from =
        if should_skip { scan.last_member_index.expect("Should be defined here") } else { 0 };

    let mut stopped = false;
    let mut archive =
        open_tar_archive(&config.archive_path, config.compression.format)?;

    // FEATURE: Switch to seek for tar
    for (member_index, entry) in archive
        .entries()
        .map_err(|e| Error::io(&config.archive_path, e))?
        .enumerate()
    {
        let member_index = member_index as u64;
        if shutdown.check_between_files().is_err() {
            stopped = true;
            break;
        }

        // INFO: iterating entries will lead to the body being consumed too (no copy to sink needed)
        if member_index < resume_from {
            entry.map_err(|e| Error::io(&config.archive_path, e))?;
            continue;
        }

        let mut entry = entry.map_err(|e| Error::io(&config.archive_path, e))?;
        let path = entry
            .path()
            .map_err(|e| Error::Other(anyhow::anyhow!("tar entry path: {e}")))?;
        let name = entry_name(&path)?;

        // TODO Verbose Logging

        process_entry(config, db_path, &local_dst, &name, &snapshot_tmp,
                      &mut db, &mut entry, &mut force_buffer, &mut scan,
        )?;

        scan.saw_any_members = true;
        scan.last_member_index = Some(member_index);

    }

    if !stopped {
        scan.scan_complete = true;
    }

    // Persist observations before any early return / interrupt propagation.
    store_progress_in_db(&mut db, &scan)?;

    if stopped { return Err(Error::Interrupted); }

    if !scan.saw_any_members { return Err(Error::Config("Archive is Empty".to_string())); }

    if db.is_none() {
        if config.force_scan {
            return Err(Error::Config(
                "Archive did not contain database. Cannot continue extraction".to_string(),
            ));
        }
        panic!(
            "INVARIANT ERROR: No Database with at least one canonical file. \
             MUST NOT exist when force_scan is false"
        );
    }

    // PRECONDITION: Archive contained at least one element and at least a database and we
    //   fully consumed teh archive.
    validate_result(&scan, resume_db)?;

    let sdb = db.expect(OPT_DB_ERROR);
    let trust_catalog = scan.from_footer || config.force_scan;
    // Mark any cache payloads that were missed during the stream.
    sdb.flush_cached_payloads(&config.extract_cache_dir())?;

    if trust_catalog {
        let n = sdb.promote_extracted_to_unarchived()?;
        if n > 0 {
            tracing::warn!(
                "promoted {n} extracted row(s) to unarchived without snapshot confirmation \
                 (footer and/or --force-scan)"
            );
        }
    }

    report_scan_completeness(&sdb, scan.from_footer, config.force_scan)?;

    let paths = sdb.count_files_in_phase(FilePhase::Unarchived)?;
    let source = if scan.from_footer {
        "footer"
    } else {
        "stream manifest"
    };

    // TODO Promote all

    tracing::info!(
        unarchived = paths,
        last_member_index = ?scan.last_member_index,
        snapshots_this_pass = scan.snapshots_ingested - snapshots_before,
        snapshots_ingested = scan.snapshots_ingested,
        "extract: catalog from {source}, {paths} path(s) unarchived"
    );

    let _ = fs::remove_file(&snapshot_tmp);
    Ok(sdb)
}

/// Fully process an entry from the tar archive.
/// Precondition: Index is valid (i.e. not extracted yet)
fn process_entry(
    config: &Config,
    db_path: &Path,
    local_dst: &Path,
    name: &str,
    snapshot_tmp: &Path,
    db: &mut Option<Database>,
    entry: &mut Entry<BufReader<Box<dyn Read>>>,
    force_buffer: &mut Option<Vec<FileId>>,
    scan: &mut ExtractScanState,
) -> Result<()> {
    match (name, scan.saw_any_members) {
        (SNAPSHOT_INIT_TAR_NAME, false) => {
            install_database(db_path, snapshot_tmp, db, entry, scan)?;
            scan.saw_manifest_db = true;
        },
        (SNAPSHOT_INIT_TAR_NAME, true) => {
            return Err(Error::Config(
                "Found manifest.sqlite not at the beginning of the archive".to_string(),
            ));
        },
        (SNAPSHOT_TAR_NAME, false) => {
            if !config.force_scan {
                return Err(Error::Config(format!(
                    "first tar member is {SNAPSHOT_TAR_NAME} not manifest.sqlite; \
                         attempt to bypass with --force-scan"
                )));
            } else {
                install_database(db_path, snapshot_tmp, db, entry, scan)?;
            }
            let ref_db = db.as_ref().expect(OPT_DB_ERROR);
            scan.snapshots_ingested = ref_db.record_snapshot_ingested()?;
        },
        (SNAPSHOT_TAR_NAME, true) => {
            if config.force_scan && db.is_none() {
                copy_database(&snapshot_tmp,entry)?;
                *db = Some(open_initial_database(snapshot_tmp, db_path)?);
                let ref_db = db.as_ref().expect(OPT_DB_ERROR);
                scan.snapshots_ingested = ref_db.record_snapshot_ingested()?;

                if let Some(buf) = force_buffer.as_mut() {
                    for fid in buf.drain(..) {
                        ref_db.mark_file_extracted(fid)?;
                    }
                }
                // Confirm from this snapshot after marking buffered extracts.
                ref_db.apply_snapshot_promote_unarchived(snapshot_tmp)?;
            } else {
                let ldb = db.as_ref().expect(OPT_DB_ERROR);
                copy_database(snapshot_tmp, entry)?;
                ldb.apply_snapshot_promote_unarchived(snapshot_tmp)?;
                scan.snapshots_ingested = ldb.record_snapshot_ingested()?;
            }
        },
        (content_id, saw_first)
        if let Ok((_, _, fid, _)) = parse_content_id(content_id) => {
            if !config.force_scan && !saw_first {
                return Err(Error::Config(format!(
                    "first tar member is canonical file {content_id} not manifest.sqlite; \
                     bypass available with --force-scan"
                )));
            }
            // PRECONDITION: either force_scan && no db is true or we saw at least one member.
            let entry_dst = local_dst.join(name);
            entry.unpack(&entry_dst).map_err(|e| Error::io(&entry_dst, e))?;

            if config.force_scan && db.is_none() {
                let buf = force_buffer.as_mut().expect(
                    "INVARIANT ERROR: Buffer must be Some(Vec) if force_scan is set",
                );
                buf.push(fid);
            } else {
                let ldb = db.as_ref().expect(OPT_DB_ERROR);
                ldb.mark_file_extracted(fid)?;
            }
        },
        (other, _) => {
            return Err(Error::Config(format!("tar member `{other}` is neither a catalog \
                nor a content id; not a tar-dedup archive?")));
        }
    }
    Ok(())
}

/// Check if the returned state at the end of scanning the archive matches our exectations and add
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

fn report_scan_completeness(
    db: &Database,
    from_footer: bool,
    force_scan: bool,
) -> Result<()> {
    // INFO: AppendedPath = true, FileExtracted = false
    let missing = db.count_missing_payloads()?;
    if missing > 0 {
        let msg = format!(
            "{missing} canonical file(s) have AppendedPath but were not extracted from the archive"
        );
        if force_scan { tracing::warn!("{msg}"); } else { return Err(Error::Config(msg)); }
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
    // INFO: ONLY canonical_id = id, FileExtracted = truer
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

/// Remove temp database and handle errors. If database does not exist, now error is emitted.
fn remove_temp_db(temp_db: &PathBuf) -> () {
    if !temp_db.exists() {
        return ();
    }

    let res = fs::remove_file(temp_db);
    if res.is_err() {
        let err = res.err().expect("MUST BE ERROR HERE");
        tracing::warn!("Failed to remove temp database with error: {err}");
    }

}

/// Store progress in db
fn store_progress_in_db(db: &mut Option<Database>, scan: &ExtractScanState) -> Result<()> {
    if let Some(d) = db {
        let mut persisted = d.load_extract_scan_state()?;
        persisted.saw_manifest_db |= scan.saw_manifest_db;
        persisted.saw_any_members |= scan.saw_any_members;
        persisted.scan_complete = scan.scan_complete;
        // `None` sorts below every `Some`, so this keeps the furthest point reached.
        persisted.last_member_index = persisted.last_member_index.max(scan.last_member_index);
        persisted.from_footer |= scan.from_footer;
        // snapshots_ingested already updated via record_snapshot_ingested.
        d.save_extract_scan_state(&persisted)?;
        d.checkpoint()?;
    }
    Ok(())
}

/// Copy the database out of the archive to `dst`.
fn copy_database<R: Read>(dst: &Path, entry: &mut R) -> Result<()> {
    let mut out = fs::File::create(dst).map_err(|e| Error::io(dst, e))?;
    copy(entry, &mut out).map_err(|e| Error::io(dst, e))?;
    Ok(())
}

/// Install catalog from `temp` into `target`, normalize, and init extract runtime state.
fn open_initial_database(temp: &Path, target: &Path) -> Result<Database> {
    Database::install_initial_manifest(temp, target)?;
    let opened = Database::open(target)?;
    opened.init_extract_runtime_state()?;
    opened.normalize_installed_catalog()?;
    Ok(opened)
}

/// Function deals with initial installation of the database (being aware of the footer)
fn install_database(
    db_path: &Path,
    snapshot_tmp: &Path,
    db: &mut Option<Database>,
    entry: &mut Entry<BufReader<Box<dyn Read>>>,
    scan: &mut ExtractScanState,
) -> Result<()> {
    if scan.from_footer {
        copy(entry, &mut std::io::sink())
            .map_err(|e| Error::io("std::io::sink()", e))?;
    } else {
        copy_database(snapshot_tmp, entry)?;
        *db = Some(open_initial_database(snapshot_tmp, db_path)?);
    }
    Ok(())
}


/// Extract and sanitize the entry basename from a tar member path.
fn entry_name(path: &Path) -> Result<String> {
    let name = path
        .clean()
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| Error::Config("invalid tar entry name".into()))?
        .to_string();
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
    use std::path::PathBuf;

    use super::*;
    use crate::common::start::StartPolicy;
    use crate::config::{
        CleanupSettings, CompressionFormat, CompressionSettings, ExtractStageLocation,
    };
    use crate::db::flags::FileFlag;
    use crate::db::types::{FilePhase, FileRecord, FileType, NewFileRecord, StrippedRecord};

    /// Catalog with one canonical regular file and one duplicate pointing at it,
    /// both `archived`, canonical flagged `AppendedPath`. Returns the member name.
    fn write_catalog(path: &Path) -> String {
        let db = Database::open(path).expect("open catalog");
        let canonical = insert(&db, "canonical.txt", 4, FileType::File);
        let duplicate = insert(&db, "duplicate.txt", 4, FileType::File);

        db.update_file_inspection(canonical, [7u8; 20], 0)
            .expect("digest");
        db.mark_self_canonical(canonical).expect("self canonical");
        db.set_canonical(duplicate, canonical).expect("canonical");
        db.set_flag(canonical, FileFlag::AppendedPath, true)
            .expect("appended");
        db.mark_file_phase(canonical, FilePhase::Archived)
            .expect("phase");
        db.mark_file_phase(duplicate, FilePhase::Archived)
            .expect("phase");

        let member = db
            .get_file::<FileRecord>(canonical)
            .expect("get")
            .expect("row")
            .tar_member_name()
            .expect("content id");
        db.checkpoint().expect("checkpoint");
        member
    }

    fn insert(db: &Database, rel_path: &str, size: u64, ftype: FileType) -> FileId {
        let path = PathBuf::from(rel_path);
        db.insert_file(&NewFileRecord {
            ext: crate::common::files::original_extension(&path),
            rel_path: path,
            size,
            mtime: None,
            atime: None,
            ctime: None,
            uid: None,
            gid: None,
            mode: None,
            ftype: Some(ftype),
            xattrs: None,
            posix_acl: None,
            selinux_ctx: None,
            link_dst: None,
        })
        .expect("insert");

        db.files_in_phase::<StrippedRecord>(FilePhase::Inventoried)
            .expect("list")
            .into_iter()
            .find(|f| f.rel_path == Path::new(rel_path))
            .expect("inserted row")
            .id
    }

    fn append_member(builder: &mut tar::Builder<fs::File>, name: &str, bytes: &[u8]) {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Regular);
        header.set_mode(0o644);
        header.set_size(bytes.len() as u64);
        header.set_cksum();
        builder.append_data(&mut header, name, bytes).expect("append");
    }

    /// `manifest.sqlite`, one payload, `snapshot.sqlite` — an uncompressed archive.
    fn build_archive(dir: &Path) -> (Config, String) {
        let catalog = dir.join("catalog.sqlite");
        let member = write_catalog(&catalog);
        let catalog_bytes = fs::read(&catalog).expect("read catalog");

        let archive_path = dir.join("archive.tar");
        let mut builder = tar::Builder::new(fs::File::create(&archive_path).expect("create tar"));
        append_member(&mut builder, SNAPSHOT_INIT_TAR_NAME, &catalog_bytes);
        append_member(&mut builder, &member, b"data");
        append_member(&mut builder, SNAPSHOT_TAR_NAME, &catalog_bytes);
        builder.finish().expect("finish tar");

        let config = Config {
            archive_path,
            input_dir: PathBuf::new(),
            output_dir: dir.join("out"),
            work_dir: dir.join("work"),
            compression: CompressionSettings::for_extract(CompressionFormat::None),
            jobs: 1,
            start_policy: StartPolicy::Auto,
            cleanup: CleanupSettings::from_flags(false, false),
            extract_stage_location: ExtractStageLocation::BesideArchive,
            exit_after_stage: None,
            restore_owner: false,
            do_xattrs: false,
            do_posix_acl: false,
            do_selinux: false,
            dedup_fail_fast: false,
            page_size: 4096,
            min_pages: Some(0),
            write_archive_footer: false,
            retry_missing_sha: false,
            force_scan: false,
            clear_archive_meta: false,
            rehash: true,
        };
        (config, member)
    }

    #[test]
    fn scan_caches_payloads_and_promotes_on_snapshot() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, member) = build_archive(dir.path());
        let db_path = config.db_path();

        let db = run(&config, &db_path, &Shutdown::detached()).expect("scan");

        assert!(config.extract_cache_dir().join(&member).is_file());
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
        assert_eq!(state.last_member_index, Some(2));
        assert_eq!(state.snapshots_ingested, 1);
        assert!(!state.from_footer);
    }

    #[test]
    fn scan_interrupt_persists_state_and_resume_skips_processed_members() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (config, _member) = build_archive(dir.path());
        let db_path = config.db_path();

        // First pass installs the catalog so later passes take the resume path.
        run(&config, &db_path, &Shutdown::detached()).expect("first scan");

        // Interrupt before the first member: state is saved, Interrupted propagates.
        let shutdown = Shutdown::detached();
        shutdown.request_graceful();
        match run(&config, &db_path, &shutdown) {
            Err(Error::Interrupted) => {}
            Err(other) => panic!("expected Interrupted, got {other:?}"),
            Ok(_) => panic!("expected Interrupted, scan returned Ok"),
        }

        let db = Database::open(&db_path).expect("open");
        let mut state = db.load_extract_scan_state().expect("state");
        assert!(!state.scan_complete);
        assert_eq!(state.last_member_index, Some(2));

        // Pretend the interrupt happened right after the leading manifest and roll
        // the catalog back, so the resumed pass has to redo members 1 and 2.
        state.last_member_index = Some(0);
        db.save_extract_scan_state(&state).expect("save");
        db.normalize_installed_catalog().expect("rollback");
        db.checkpoint().expect("checkpoint");
        drop(db);

        // Resuming must skip the manifest member; re-reading it would be an error.
        let db = run(&config, &db_path, &Shutdown::detached()).expect("resumed scan");
        assert_eq!(
            db.count_files_in_phase(FilePhase::Unarchived).expect("count"),
            2
        );
        let state = db.load_extract_scan_state().expect("state");
        assert!(state.scan_complete);
        assert_eq!(state.last_member_index, Some(2));
        // One from the first pass, one from the resumed pass; the interrupt added none.
        assert_eq!(state.snapshots_ingested, 2);
    }
}
