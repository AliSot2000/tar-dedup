use walkdir::WalkDir;

use crate::archive::ArchiveRTArgs;
use crate::common::files::{determine_file_type, original_extension};
#[cfg(windows)]
use crate::common::files::get_file_times;
use crate::common::utf8::{SanitizedPath, sanitize_path};
use crate::common::xattr::{get_file_acl, get_file_selinux_data, get_file_xattr};
use crate::db::ErrorPhase;
use crate::db::Recorder;
use crate::db::flags::{ErrorFlags, SourceFlag, SourceFlags};
use crate::db::types::{FileType, NewFileRecord};
use crate::error::{Error, FileStatError, Result};
use crate::progress::BarKind;
use chrono::{DateTime, Utc};
use dedent::dedent;
use indicatif::ProgressBar;
use path_clean::PathClean;
use std::ffi::OsStr;
use std::fs::Metadata;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::{fs, io};

const ERROR_PHASE: ErrorPhase = ErrorPhase::Pipeline(crate::config::PipelinePhase::Inventory);
const SUB_BAR_PREFIX: &str = "Scanning Entries in: ";

pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    let total_sources = rt.config.inputs.input_dirs.len() + rt.config.inputs.files_from.len();
    debug_assert!(rt.config.inputs.input_dirs.len() + rt.config.inputs.files_from.len() > 0,
        "PRECONDITION FAILED: Expected at least one element across --files-from and --input-dir");

    if rt.db.count_entries()? > 0 {
        tracing::warn!("Found interrupted scan of directories. Purging and starting again.");
        rt.db.purge_entries()?;
    }

    tracing::info!(
        "starting inventory: total {}, processing {}, pending {}",
        rt.db.count_entries()?,
        total_sources,
        total_sources,
    );

    // TOOD dedent figure out.
    tracing::info!(dedent!("
        Inventory pass cannot be gracefully interrupted. \
        If force aborted, the inventory pass needs to be run again to ensure \
        consistent snapshot of filesystem."));
    let mut processed = 0u64;
    rt.progress.set_table_size(total_sources as u64);
    let sub_bar = rt.progress.push_sub_bar("Scanned Entries", BarKind::Counter);

    // One recorder for the whole pass; auto-flush bounds the buffer on error-heavy
    // filesystems, and the final flush happens on drop even if the pass aborts.
    let mut recorder = Recorder::new(rt.db, !rt.config.process.no_errors, Some(rt.config.process.auto_flush_limit));

    // Handle input directories
    // INFO: Already sanitized those dirs, that they dont overlap.
    for (index, input_dir) in rt.config.inputs.input_dirs.iter().enumerate() {
        rt.shutdown.check_in_flight()?;
        sub_bar.set_position(0);
        sub_bar.set_message(format!("{SUB_BAR_PREFIX}'{}'", input_dir.absolute_path.display()));

        tracing::info!(root = %input_dir.absolute_path.display(), "inventory pass");

        // Sanity checks
        debug_assert!(input_dir.absolute_path.is_dir(),
                      "Input Dir must contain valid directories");
        debug_assert!(input_dir.absolute_path.is_absolute(),
                      "Input Dir must be absolute");
        debug_assert!(input_dir.absolute_path.clean() == input_dir.absolute_path,
                      "Path should be minimal");

        // INFO: Check needed because source assumes everything serializes correctly!
        let Some(san_org) = sanitize_path(
            rt.config.process.relax_utf8, rt.config.process.fail_fast,
            &input_dir.original_path, "input-dir-org")? else { continue };
        let Some(san_abs) = sanitize_path(
            rt.config.process.relax_utf8, rt.config.process.fail_fast,
            &input_dir.absolute_path, "input-dir-abs")? else { continue };

        let source_id = rt.db.add_get_source(
            Path::new(&san_abs.text),
            "--input-dir",
            Some(index as u64),
            Path::new(&san_org.text),
            &san_abs.raw,
            &san_org.raw,
            SourceFlags::default().with(SourceFlag::IsDirectory, true),
        )?;
        handle_dir(&rt, &sub_bar, source_id, &input_dir.absolute_path,
                   &mut processed, &mut recorder)?;
        rt.progress.inc_global(1);
    }

    // Handle from-files
    for files_file in rt.config.inputs.files_from.iter() {
        // The from-file is a source too: strict skips a non-UTF-8 path, relax
        // lossy-converts (only the display name is affected; the file is read
        // by its raw bytes below).
        if sanitize_path(
            rt.config.process.relax_utf8, rt.config.process.fail_fast,
            files_file, "files-from")?.is_none() {
            rt.progress.inc_global(1);
            continue;
        }
        sub_bar.set_position(0);
        sub_bar.set_message(format!("{SUB_BAR_PREFIX}'{}'", files_file.display()));

       // Deal with files from stdin.
        if files_file.to_path_buf() == PathBuf::from("-") {
            let br = BufReader::new(io::stdin());
            let ff_iter = files_from_reader(br, rt.config.inputs.files_from_null);
            for element in ff_iter {
                let (line, result) = element;
                let path = match result {
                    Ok(path_vec) => PathBuf::from(&os_str_from_bytes(&path_vec)),
                    Err(e) => {
                        let error = Error::io("-", e);
                        recorder.record_session(
                            ERROR_PHASE, error.to_only_file_stat().unwrap(), ErrorFlags::default());
                        if rt.config.process.fail_fast {
                            return Err(error)
                        } else {
                            tracing::error!("Processing input failed: {error}");
                            rt.progress.inc_global(1);
                            continue
                        }

                    },
                };

                handle_from_files_line(&rt, &sub_bar, (line, &path), &files_file,
                                       &mut processed, &mut recorder)?
            }
            rt.progress.inc_global(1);
            continue;
        }
        // Sanity check
        debug_assert!(files_file.is_file(),"Input Dir must contain valid directories");
        debug_assert!(files_file.is_absolute(), "Input Dir must be absolute");
        debug_assert!(files_file.clean() == files_file.to_path_buf(), "Path should be minimal");

        let file = match fs::read(&files_file) {
            Ok(f) => f,
            Err(e) => {
                recorder.record_session(
                    ERROR_PHASE,
                    FileStatError::copy_io(&files_file, &e),
                    ErrorFlags::default());
                return Err(Error::io(&files_file, e));
            }
        };

        for element in files_from_records(&file, rt.config.inputs.files_from_null) {
            let (line, path) = element;
            handle_from_files_line(&rt, &sub_bar, (line, &path), &files_file,
                                   &mut processed, &mut recorder)?
        }
        rt.progress.inc_global(1);
    }

    // Set hardlink canonicals if and only if, we want to collapse the hardlinks and
    if !rt.config.indexing.no_hardlink_detection {
        let rows = rt.db.set_hardlink_canonicals()?;
        tracing::info!("Updated {rows} of hardlink groups to have one canonical");
    }

    if !rt.config.capture.numeric_ids_only {
        rt.db.resolve_numeric_ids(&mut recorder)?;
    }
    recorder.flush()?;
    let missing_dev_ino_count = rt.db.count_missing_dev_inode()?;
    if missing_dev_ino_count > 0 {
        tracing::warn!("Encountered {missing_dev_ino_count} files without dev or inode information.\
         Those files won't be captured by hardlink detection");
    }
    // TODO: Add inspect with query for missing files. Ha???
    tracing::info!(
        "inventory done: processed {} of {}, {} error(s)",
        processed,
        rt.db.count_entries()?,
        rt.db.count_errors(Some("inventory"))?,
    );
    let failed_implication = rt.db.count_id_implication()?;
    assert_eq!(failed_implication, 0, "INVARIANT FAILED: Number of files with username / groupname \
        set but not uid / gid set MUST be 0");

    Ok(())
}

/// Process a single line from the --from-files argument
fn handle_from_files_line<P: AsRef<Path>>(
    rt: &ArchiveRTArgs,
    sub_bar: &ProgressBar,
    element: (usize, P),
    from_files_path: &Path,
    processed: &mut u64,
    recorder: &mut Recorder)
    -> Result<()> {

    let (line, ff) = element;
    let fpath: &Path = ff.as_ref();
    let from_files_disp_path = from_files_path.display();

    let abs_path = if fpath.is_absolute() {
        fpath.to_path_buf().clean()
    } else {
        rt.config.paths.directory.join(fpath).clean()
    };
    debug_assert!(abs_path.is_absolute(), "Path must be absolute now");

    // INFO: Check needed because source assumes everything serializes correctly!
    let Some(san_fpath) = sanitize_path(
        rt.config.process.relax_utf8, rt.config.process.fail_fast,
        &fpath.clean(), "line-from-stdin")? else { return Ok(()) };
    let Some(san_abs) = sanitize_path(
        rt.config.process.relax_utf8, rt.config.process.fail_fast,
        &abs_path, "conv-abs-from-stdin")? else { return Ok(()) };

    if abs_path.is_dir() {
        if let Some((_, existing)) = rt.db.find_overlapping_source(
            Path::new(&san_abs.text), rt.config.indexing.no_recursion)? {

            let err_str = format!(
                "input directory `{}` overlaps `{}`; use `--no-strict-separation` to walk anyway",
                Path::new(&san_abs.text).display(),
                existing.display()
            );
            if !rt.config.indexing.no_strict_separation {
                return Err(Error::Config(err_str));
            } else {
                tracing::warn!(err_str);
            }
        }
    }

    let source_id = rt.db.add_get_source(
        Path::new(&san_abs.text),
        &format!("--files-from={from_files_disp_path}"),
        Some(line as u64),
        Path::new(&san_fpath.text),
        &san_abs.raw,
        &san_fpath.raw,
        SourceFlags::default().with(SourceFlag::IsDirectory, abs_path.is_dir()),
    )?;
    handle_dir(&rt, &sub_bar, source_id, &abs_path, processed, recorder)?;

    Ok(())
}

/// Handle a single dir by walking the directory or the directory tree
/// PRECONDITION:
/// - Directory exists
/// - Path is minimal
/// - Path is directory.
/// - Path is on the same file system if called recursively
fn handle_dir(
    rt: &ArchiveRTArgs,
    sub_bar: &ProgressBar,
    source_id: i64,
    start_dir: &Path,
    processed: &mut u64,
    recorder: &mut Recorder)
    -> Result<()> {

    let iter = WalkDir::new(&start_dir)
        .follow_links(rt.config.indexing.dereference)
        .follow_root_links(true) // INFO: Custom handling by us
        .same_file_system(rt.config.indexing.one_file_system)
        .min_depth(0)
        .max_depth(if rt.config.indexing.no_recursion { 1 } else { usize::MAX })
        .contents_first(false)
        .into_iter();

    for element in iter {
        rt.shutdown.check_in_flight()?;
        let entry = match element {
            Err(e) => {
                // Failed to access a single element while walking; the file row
                // may or may not exist, so this is recorded without a file_id.
                recorder.record_session(
                    ERROR_PHASE,
                    FileStatError::General {
                        path: Some(start_dir.to_path_buf()),
                        message: format!("Failed to access element in path with error: {e}"),
                    },
                    ErrorFlags::default(),
                );
                if rt.config.process.fail_fast {
                    return Err(Error::Config("Failed to access element with error: {e}".into()));
                } else {
                    tracing::error!("Failed to access element with error: {e}");
                    continue;
                }
            }
            Ok(entry) => entry,
        };
        handle_entry_base(&rt, sub_bar, &entry.path(), source_id, processed, recorder)?;
    }
    Ok(())
}

fn handle_entry_base(
    rt: &ArchiveRTArgs,
    sub_bar: &ProgressBar,
    path: &Path,
    source_id: i64,
    processed: &mut u64,
    recorder: &mut Recorder)
    -> Result<()> {
    debug_assert!(path.is_absolute(), "Expected Absolute paths only.");

    // UTF-8 gate: strict skips the entry (warn), strict + fail-fast aborts,
    // relax lossy-converts (text) while keeping the raw bytes for fs access.
    let Some(san) = sanitize_path(
        rt.config.process.relax_utf8, rt.config.process.fail_fast,
        path, "inventory-entry")? else { return Ok(()); };

    // Preflight: already inventoried — attach this source without restatting.
    if let Some(file_id) = rt.db.file_id_by_abs_path(&san.raw)? {
        rt.db.add_ref(source_id, file_id)?;
        return Ok(());
    }

    handle_entry(&rt, &sub_bar, &san, source_id, processed, recorder)
}

/// PRECONDITION: path is abs.
#[cfg(unix)]
fn handle_entry(
    rt: &ArchiveRTArgs,
    sub_bar: &ProgressBar,
    san: &SanitizedPath,
    source_id: i64,
    processed: &mut u64,
    recorder: &mut Recorder)
    -> Result<()> {
    // `san` bundles the DB text form and the raw on-disk bytes; filesystem access
    // always goes through the raw bytes (`fs_path()` once a record exists).
    let path: &Path = san.raw.as_path();
    debug_assert!(path.is_absolute(), "PRECONDITION FAILED: path should always be abs");
    use std::os::unix::fs::MetadataExt;

    let mut enc_err = Vec::new();
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            // File row cannot be created without metadata; record with the path
            // alone so the failure is not lost when the phase aborts.
            recorder.record_session(
                ERROR_PHASE,
                FileStatError::Io {
                    path: PathBuf::from(&san.text),
                    source: io::Error::new(e.kind(), e.to_string()),
                },
                ErrorFlags::default(),
            );
            // The recorder is shared per-pass; flush everything before returning
            // the error so this record is not lost to the abort.
            recorder.flush()?;
            return Err(Error::io(path, e));
        }
    };

    let mtime_s = meta.mtime();
    let mtime_nsec = meta.mtime_nsec();
    debug_assert!((0..1_000_000_000).contains(&mtime_nsec));
    let mtime: Option<DateTime<Utc>> = DateTime::from_timestamp(mtime_s, mtime_nsec as u32);
    if mtime.is_none() {
        tracing::warn!(
            "File {} has Implausible Timestamp.  {mtime_s}s, {mtime_nsec}nsec", path.display());
        enc_err.push(FileStatError::general(
            Some(path),
            format!("Implausible mtime: {mtime_s}s, {mtime_nsec}nsec")));
    }

    let atime_s = meta.atime();
    let atime_nsec = meta.atime_nsec();
    debug_assert!((0..1_000_000_000).contains(&atime_nsec));
    let atime: Option<DateTime<Utc>> = DateTime::from_timestamp(atime_s, atime_nsec as u32);
    if atime.is_none() {
        tracing::warn!(
            "File {} has Implausible Timestamp.  {atime_s}s, {atime_nsec}nsec", path.display());
        enc_err.push(FileStatError::general(
            Some(path),
            format!("Implausible atime: {atime_s}s, {atime_nsec}nsec")));
    }

    let ctime_s = meta.mtime();
    let ctime_nsec = meta.mtime_nsec();
    debug_assert!((0..1_000_000_000).contains(&ctime_nsec));
    let ctime: Option<DateTime<Utc>> = DateTime::from_timestamp(ctime_s, ctime_nsec as u32);
    if ctime.is_none() {
        tracing::warn!(
            "File {} has Implausible Timestamp.  {ctime_s}s, {ctime_nsec}nsec", path.display());
        enc_err.push(FileStatError::general(
            Some(path),
            format!("Implausible ctime: {ctime_s}s, {ctime_nsec}nsec")));
    }

    let mode = meta.mode();
    let uid = meta.uid();
    let gid = meta.gid();
    let dev = meta.dev();
    let ino = meta.ino();

    let ftype = inner_file_type(&meta, &path, &mut enc_err);

    // Symlink targets follow the same UTF-8 policy: strict skips the entry (a
    // non-UTF-8 target cannot be stored), relax stores the lossy target text plus
    // the original bytes (`link_dst_raw`, doc-only).
    let (link_dst, link_dst_raw, major, minor) = match ftype {
        FileType::Symlink(_) => {
            match strip_transpose(path, fs::read_link(path), &mut enc_err) {
                None => (None, None, None, None),
                Some(dst) => {
                    let Some(link_san) = sanitize_path(
                        rt.config.process.relax_utf8,
                        rt.config.process.fail_fast,
                        &dst,
                        "link-target")? else { return Ok(()); };
                    (Some(PathBuf::from(link_san.text)), Some(link_san.raw), None, None)
                }
            }
        }
        FileType::CharacterDevice | FileType::BlockDevice => {
            let (maj, min) = get_file_rdev_parts(&meta);
            (None, None, maj, min)
        }
        FileType::Unknown => {
            // INFO: Error not stored, since it is obvious from unknown.
            tracing::error!("{} could not be classified into a valid file type.", path.display());
            (None, None, None, None)
        }
        _ => (None, None, None, None),
    };

    // Optional data
    let xattrs = if rt.config.capture.do_xattrs {
        match get_file_xattr(path) {
            Err(e) => { enc_err.push(e); None }
            Ok(md) => Some(md),
        }
    } else { None };
    let posix_acl = if rt.config.capture.do_posix_acl {
        match get_file_acl(path) {
            Err(e) => { enc_err.push(e); None},
            Ok(md) => Some(md),
        }
    } else { None };
    let selinux_ctx = if rt.config.capture.do_selinux {
        match get_file_selinux_data(path) {
            Err(e) => { enc_err.push(e); None},
            Ok(md) => Some(md),
        }
    } else { None };

    if rt.db.insert_file_and_ref(
        source_id,
        &NewFileRecord {
            abs_path: Path::new(&san.text).clean().to_path_buf(),
            abs_path_raw: Some(san.raw.clone()),
            ext: original_extension(&path),
            size: meta.len(),
            mtime,
            atime,
            ctime,
            uid: Some(uid),
            gid: Some(gid),
            ftype: Some(ftype),
            mode: Some(mode),
            xattrs,
            posix_acl,
            selinux_ctx,
            win_perm: None,
            link_dst,
            link_dst_raw,
            device_id: Some(dev),
            inode_id: Some(ino),
            major,
            minor,
        },
    )? {
        *processed += 1;
        sub_bar.inc(1);
        tracing::debug!("inventoried {}", path.display());
    }

    // Persist the accumulated per-file errors (xattr/ACL/SELinux/ftype/times/read-link).
    if !enc_err.is_empty() {
        // Row was just inserted above, so the id must exist.
        let file_id = rt.db.file_id_by_abs_path(path)?.expect(
            "INVARIANT ERROR: file was inserted, id lookup must succeed");
        for error in enc_err {
            recorder.record_file(
                file_id,
                ERROR_PHASE,
                error,
                ErrorFlags::default(),
            );
        }
    }
    Ok(())
}

/// Handle a single dir entry.
/// PRECONDITION: path is abs.
#[cfg(windows)]
fn handle_entry(
    rt: &ArchiveRTArgs,
    sub_bar: &ProgressBar,
    san: &SanitizedPath,
    source_id: i64,
    processed: &mut u64,
    recorder: &mut Recorder)
    -> Result<()> {
    let path: &Path = san.raw.as_path();
    let mut enc_err = Vec::new();
    debug_assert!(path.is_absolute(), "PRECONDITION FAILED: path should always be abs");

    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            // File row cannot be created without metadata; record with the path
            // alone so the failure is not lost when the phase aborts.
            recorder.record_session(
                ERROR_PHASE,
                FileStatError::Io {
                    path: PathBuf::from(&san.text),
                    source: io::Error::new(e.kind(), e.to_string()),
                },
                ErrorFlags::default(),
            );
            // The recorder is shared per-pass; flush everything before returning
            // the error so this record is not lost to the abort.
            recorder.flush()?;
            return Err(Error::io(path, e));
        }
    };

    // Extract times, retaining the errors.
    let times = get_file_times(&meta);
    let mtime = strip_transpose(path, times.0, &mut enc_err);
    let atime = strip_transpose(path, times.1, &mut enc_err);
    let ctime = strip_transpose(path, times.2, &mut enc_err);
    let uid = None;
    let gid = None;
    let ftype = inner_file_type(&meta, &path, &mut enc_err);

    // Volume serial number + file index fill the `(dev, inode)` tuple; this is
    // what feeds hardlink detection and the dedup pre-flight check.
    let dev = strip_transpose(path, get_file_dev(&meta), &mut enc_err);
    let ino = strip_transpose(path, get_file_ino(&meta), &mut enc_err);

    let link_dst: Option<PathBuf> = if matches!(ftype, FileType::Symlink(_)) {
        strip_transpose(path, fs::read_link(path), &mut enc_err)
    } else {
        None
    };

    // No device nodes on Windows.
    let major = None;
    let minor = None;

    // Optional data. POSIX xattrs / ACLs / SELinux have no mapping here; the
    // NTFS attributes are captured as a JSON blob instead.
    let xattrs = None;
    let posix_acl = None;
    let selinux_ctx = None;
    let win_perm = Some(get_file_win_perms(&meta));

    if db.insert_file_and_ref(
        source_id,
        &NewFileRecord {
            abs_path: Path::new(&san.text).clean().to_path_buf(),
            ext: original_extension(&path),
            size: meta.len(),
            mtime,
            atime,
            ctime,
            uid,
            gid,
            ftype: Some(ftype),
            mode: Some(file_mode(&meta, &ftype)),
            xattrs,
            posix_acl,
            selinux_ctx,
            win_perm,
            link_dst: link_dst.clone(),
            abs_path_raw: None,
            link_dst_raw: None,
            device_id: dev,
            inode_id: ino,
            major,
            minor,
        },
    )? {
        *processed += 1;
        sub_bar.inc(1);
        tracing::debug!("inventoried {}", path.display());
    }
    // Persist the accumulated per-file errors (xattr/ACL/SELinux/ftype/times/read-link).
    if !enc_err.is_empty() {
        // Row was just inserted above, so the id must exist.
        let file_id = rt.db.file_id_by_abs_path(path)?.expect(
            "INVARIANT ERROR: file was inserted, id lookup must succeed");
        for error in enc_err {
            recorder.record_file(
                file_id,
                ERROR_PHASE,
                error,
                ErrorFlags::default(),
            );
        }
    }
    Ok(())
}

/// Interpret a raw `-T` record as an `OsStr`. Unix keeps the lossless bytes;
/// Windows (UTF-16 `OsStr`) treats the list as UTF-8 text with lossy fallback.
fn os_str_from_bytes(bytes: &[u8]) -> std::borrow::Cow<'_, OsStr> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        std::borrow::Cow::Borrowed(OsStr::from_bytes(bytes))
    }
    #[cfg(windows)]
    {
        let text = String::from_utf8_lossy(bytes);
        std::borrow::Cow::Owned(OsStr::new(text.as_ref()).to_os_string())
    }
}

fn strip_transpose<T>(path: &Path, source: io::Result<T>, errors: &mut Vec<FileStatError>)
    -> Option<T> {
    match source {
        Err(e) => {
            errors.push(FileStatError::Io {
                path: path.to_path_buf(),
                source: e,
            });
            None
        }
        Ok(dt_utc) => Some(dt_utc),
    }
}

/// Split an arbitrary file into slices which are
/// - delimited by either `\0` or `\n`
/// - not empty (blank lines / trailing separator)
fn files_from_records(buf: &[u8], null: bool) -> impl Iterator<Item = (usize, PathBuf)> + '_ {
    let sep = if null { b'\0' } else { b'\n' };
    buf.split(move |&b| b == sep)
        .enumerate()
        .map(|(line, rec)| (line, rec.strip_suffix(b"\r").unwrap_or(rec)))
        .map(|(line, rec)| (line, PathBuf::from(&os_str_from_bytes(rec))))
        .filter(|(_line, rec)| !rec.as_os_str().is_empty())
}
/// Split an arbitrary Buffer into slices which are
/// - delimited by either `\0` or `\n`
/// - not empty (blank lines / trailing separator)
fn files_from_reader(mut reader: impl BufRead, null: bool)
    -> impl Iterator<Item = (usize, io::Result<Vec<u8>>)> {
    let sep = if null { b'\0' } else { b'\n' };
    std::iter::from_fn(move || {
        let mut rec = Vec::new();
        match reader.read_until(sep, &mut rec) {
            Ok(0) => None, // EOF → list finished
            Ok(_) => {
                if rec.last() == Some(&sep) {
                    rec.pop();
                }
                if !null && rec.last() == Some(&b'\r') {
                    rec.pop();
                }
                Some(Ok(rec))
            }
            Err(e) => Some(Err(e)),
        }
    })
    .enumerate()
    .filter(|(_line, res)| match res {
        Ok(rec) => !rec.is_empty(),
        Err(_) => true, // keep errors so the loop can handle them
    })
}

/// Synthesize a POSIX-style mode from the NTFS attributes. Directories read as
/// `rwxr-xr-x`, regular files as `rw-r--r--`, symlinks as `rwxrwxrwx`; the
/// read-only attribute clears the write bits (`0o222`).
#[cfg(windows)]
fn file_mode(meta: &fs::Metadata, ftype: &FileType) -> u32 {
    use std::os::windows::fs::MetadataExt;

    // FILE_ATTRIBUTE_READONLY
    const FILE_ATTRIBUTE_READONLY: u32 = 0x1;

    let base = match ftype {
        FileType::Directory => 0o755,
        FileType::Symlink(_) => 0o777,
        _ => 0o644,
    };
    if meta.file_attributes() & FILE_ATTRIBUTE_READONLY != 0 {
        base & !0o222
    } else {
        base
    }
}

#[cfg(windows)]
fn get_file_dev(meta: &fs::Metadata) -> io::Result<u64> {
    use std::os::windows::fs::MetadataExt;
    meta.volume_serial_number()
        .map(|v| v as u64)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                "volume serial number is not available",
            )
        })
}

#[cfg(windows)]
fn get_file_ino(meta: &fs::Metadata) -> io::Result<u64> {
    use std::os::windows::fs::MetadataExt;
    meta.file_index()
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "file index is not available"))
}

/// Serialize the NTFS `FILE_ATTRIBUTE_*` bitmask as a small JSON document so it
/// can be round-tripped during restore (`win_perm` column). NULL on unix.
#[cfg(windows)]
fn get_file_win_perms(meta: &fs::Metadata) -> String {
    use std::os::windows::fs::MetadataExt;
    serde_json::json!({ "attributes": meta.file_attributes() }).to_string()
}

#[cfg(unix)]
fn get_file_rdev_parts(meta: &fs::Metadata) -> (Option<u64>, Option<u64>) {
    use std::os::unix::fs::MetadataExt;
    let rdev = meta.rdev();
    (
        Some(nix::sys::stat::major(rdev)),
        Some(nix::sys::stat::minor(rdev)),
    )
}

fn inner_file_type(meta: &Metadata, path: &Path, enc_err: &mut Vec<FileStatError>) -> FileType {
    match determine_file_type(&meta, &path) {
        Ok(t) => t,
        Err((t, _failed_path, e)) => {
            enc_err.push(FileStatError::Io {
                path: path.to_path_buf(),
                source: e,
            });
            t
        }
    }
}

//==================================================================================================
// Testing
//==================================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use crate::config::{ArchiveConfig, PathSource};
    use crate::db::flags::FileFlag;
    use crate::db::types::{FileId, FilePhase, FileRecord, LinkType, StrippedRecord};
    use crate::test_support::{TestWorld, insert};
    use clap::Parser;
    use nix::unistd::geteuid;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::{Path, PathBuf};

    // ---- helpers ----------------------------------------------------------------

    fn sub_bar(world: &TestWorld) -> ProgressBar {
        world.progress.push_sub_bar("t", BarKind::Counter)
    }

    fn recorder<'a>(world: &'a TestWorld) -> Recorder<'a> {
        Recorder::new(&world.db, true, None)
    }

    fn add_source(world: &TestWorld, abs: &Path) -> i64 {
        world.db.add_get_source(
            abs, "test", Some(1), abs, abs, abs,
            SourceFlags::default().with(SourceFlag::IsDirectory, abs.is_dir()),
        ).expect("add source")
    }

    fn ref_count(world: &TestWorld, id: FileId) -> u64 {
        world.db.with_transaction(|conn| {
            let n: i64 = conn.query_row(
                "SELECT COUNT(*) FROM ref WHERE file_id = :id",
                rusqlite::named_params! { ":id": id.0 },
                |r| r.get(0),
            ).expect("count refs");
            Ok(n as u64)
        }).expect("ref count")
    }

    fn rows_under(world: &TestWorld, prefix: &Path) -> Vec<PathBuf> {
        world.db.files_in_phase::<StrippedRecord>(FilePhase::Inventoried).expect("rows")
            .into_iter()
            .map(|r| r.abs_path)
            .filter(|p| p.starts_with(prefix))
            .collect()
    }

    fn non_utf8(name: &[u8]) -> PathBuf {
        PathBuf::from(OsString::from_vec(name.to_vec()))
    }

    fn non_utf8_dir(world: &TestWorld) -> PathBuf {
        let p = world.dir.path().join(OsString::from_vec(b"bad-\xFF\xFE".to_vec()));
        fs::create_dir_all(&p).expect("create non-utf8 dir");
        p
    }

    /// `{a.txt, sub/{b.txt, c.txt}}` (5 entries recursive, 3 non-recursive).
    fn make_tree(world: &TestWorld) -> PathBuf {
        let root = world.path("tree");
        fs::create_dir_all(root.join("sub")).expect("mkdir sub");
        fs::write(root.join("a.txt"), b"a").expect("write a");
        fs::write(root.join("sub/b.txt"), b"b").expect("write b");
        fs::write(root.join("sub/c.txt"), b"c").expect("write c");
        root
    }

    fn dir_source(path: &Path) -> PathSource {
        PathSource {
            original_path: path.to_path_buf(),
            absolute_path: path.to_path_buf(),
        }
    }

    fn write_list(world: &TestWorld, name: &str, lines: &[&str], null: bool) -> PathBuf {
        let p = world.path(name);
        let sep: &[u8] = if null { b"\0" } else { b"\n" };
        let mut buf = Vec::new();
        for l in lines {
            buf.extend_from_slice(l.as_bytes());
            buf.extend_from_slice(sep);
        }
        fs::write(&p, buf).expect("write list");
        p
    }

    /// `--input-dir` / `-T` overlap matrix through real clap parsing +
    /// `ArchiveConfig::build`. Returns Ok for valid inputs, Err for overlaps.
    fn build_overlap(a: &Path, b: &Path, no_recursion: bool) -> Result<ArchiveConfig> {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = dir.path().join("test.astar");
        let mut argv = vec![
            "tar-dedup".to_string(), "archive".to_string(),
            "-f".to_string(), archive.to_string_lossy().into_owned(),
        ];
        if no_recursion {
            argv.push("--no-recursion".to_string());
        }
        for d in [a, b] {
            argv.push("-i".to_string());
            argv.push(d.to_string_lossy().into_owned());
        }
        let cli = Cli::try_parse_from(argv).expect("parse cli");
        match cli.command {
            Command::Archive(args) => ArchiveConfig::build(&args, None),
            _ => panic!("expected archive command"),
        }
    }

    fn run_entry(world: &TestWorld, path: &Path, source_id: i64) -> FileId {
        let san = sanitize_path(false, false, path, "t").expect("san").expect("san present");
        let mut processed = 0u64;
        let mut rec = recorder(world);
        let bar = sub_bar(world);
        handle_entry(&world.rt(), &bar, &san, source_id, &mut processed, &mut rec).expect("ok");
        rec.flush().expect("flush");
        world.db.file_id_by_abs_path(path).expect("lookup").expect("row present")
    }

    // ---- restart purge ----------------------------------------------------------

    #[test]
    fn restart_purges_previous_result() {
        let mut world = TestWorld::new();
        let tree = make_tree(&world);
        // A stale row from an "interrupted" prior inventory, outside the walked tree.
        insert(&world.db, "/stale/old.txt", 3, FileType::File);
        assert_eq!(world.db.count_entries().expect("count"), 1);

        world.config.inputs.input_dirs = vec![dir_source(&tree)];
        run(&world.rt()).expect("restart run");

        assert_eq!(world.db.count_entries().expect("count"), 5);
        assert!(world.db.file_id_by_abs_path(Path::new("/stale/old.txt")).expect("lookup").is_none());
        assert!(world.db.file_id_by_abs_path(&tree.join("a.txt")).expect("lookup").is_some());
    }

    // ---- input_dir scans ---------------------------------------------------------

    #[test]
    fn input_scan_recursive() {
        let mut world = TestWorld::new();
        let tree = make_tree(&world);
        world.config.inputs.input_dirs = vec![dir_source(&tree)];
        run(&world.rt()).expect("run");
        assert_eq!(world.db.count_entries().expect("count"), 5);
    }

    #[test]
    fn input_scan_non_recursive() {
        let mut world = TestWorld::new();
        let tree = make_tree(&world);
        world.config.inputs.input_dirs = vec![dir_source(&tree)];
        world.config.indexing.no_recursion = true;
        run(&world.rt()).expect("run");
        assert_eq!(world.db.count_entries().expect("count"), 3);
    }

    #[test]
    fn input_scan_nested_non_recursive() {
        let mut world = TestWorld::new();
        let tree = make_tree(&world);
        world.config.inputs.input_dirs = vec![
            dir_source(&tree),
            dir_source(&tree.join("sub")),
        ];
        world.config.indexing.no_recursion = true;
        run(&world.rt()).expect("run");
        // distinct trees non-recursively: 5 unique rows. `sub` is a child of the
        // first tree AND the root of the second -> 2 refs; `b` only sits in the
        // second tree -> 1 ref (the first, non-recursive walk never descends).
        assert_eq!(world.db.count_entries().expect("count"), 5);
        let sub = world.db.file_id_by_abs_path(&tree.join("sub")).expect("lookup").expect("sub row");
        let b = world.db.file_id_by_abs_path(&tree.join("sub/b.txt")).expect("lookup").expect("b row");
        assert_eq!(ref_count(&world, sub), 2);
        assert_eq!(ref_count(&world, b), 1);
    }

    // ---- overlap matrix (CLI / config level) -------------------------------------

    #[test]
    fn overlap_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ab = dir.path().join("a/b");
        fs::create_dir_all(&ab).expect("mkdir");
        let msg = build_overlap(&ab, &ab, false).expect_err("overlap").to_string();
        assert!(msg.contains("overlaps"), "{msg}");
        let msg = build_overlap(&ab, &ab, true).expect_err("overlap").to_string();
        assert!(msg.contains("overlaps"), "{msg}");
    }

    #[test]
    fn overlap_nested_recursion_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ab = dir.path().join("a/b");
        let abc = ab.join("c");
        fs::create_dir_all(&abc).expect("mkdir");
        // Nested roots overlap only with recursion (both argument orders).
        assert!(build_overlap(&abc, &ab, false).is_err());
        assert!(build_overlap(&ab, &abc, false).is_err());
        // `--no-recursion` walks distinct trees.
        assert!(build_overlap(&abc, &ab, true).is_ok());
        assert!(build_overlap(&ab, &abc, true).is_ok());
    }

    #[test]
    fn overlap_siblings_valid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ab = dir.path().join("a/b");
        let ac = dir.path().join("a/c");
        let abc = ab.join("c");
        fs::create_dir_all(&abc).expect("mkdir");
        fs::create_dir_all(&ac).expect("mkdir");
        for no_recursion in [false, true] {
            for (x, y) in [(&ab, &ac), (&abc, &ac)] {
                assert!(build_overlap(x, y, no_recursion).is_ok());
                assert!(build_overlap(y, x, no_recursion).is_ok());
            }
        }
    }

    // ---- files_from ----------------------------------------------------------------

    #[test]
    fn files_from_run_newline() {
        let mut world = TestWorld::new();
        let tree = make_tree(&world);
        let list = write_list(&world, "list.txt", &[
            tree.join("a.txt").to_str().unwrap(),
            tree.join("sub").to_str().unwrap(),
            "", // blank line is dropped
        ], false);
        world.config.inputs.files_from = vec![list];
        run(&world.rt()).expect("run");
        // a.txt (file line) + sub walked as a dir (sub, b, c) = 4.
        assert_eq!(world.db.count_entries().expect("count"), 4);
    }

    #[test]
    fn files_from_run_null() {
        let mut world = TestWorld::new();
        let tree = make_tree(&world);
        let list = write_list(&world, "list0.txt", &[
            tree.join("a.txt").to_str().unwrap(),
            tree.join("sub/b.txt").to_str().unwrap(),
        ], true);
        world.config.inputs.files_from = vec![list];
        world.config.inputs.files_from_null = true;
        run(&world.rt()).expect("run");
        assert_eq!(world.db.count_entries().expect("count"), 2);
    }

    #[test]
    fn files_from_records_newline_and_cr() {
        let items: Vec<PathBuf> = files_from_records(b"one\ntwo\r\n\nthree\n", false)
            .map(|(_l, p)| p).collect();
        assert_eq!(items, vec![
            PathBuf::from("one"), PathBuf::from("two"), PathBuf::from("three"),
        ]);
    }

    #[test]
    fn files_from_reader_newline_and_cr() {
        let items: Vec<Vec<u8>> = files_from_reader(BufReader::new(&b"one\ntwo\r\n\nthree\n"[..]), false)
            .map(|(_l, r)| r.expect("ok")).collect();
        assert_eq!(items, vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]);
    }

    #[test]
    fn files_from_reader_null_separated() {
        let items: Vec<Vec<u8>> = files_from_reader(BufReader::new(&b"one\0two\0\0three\0"[..]), true)
            .map(|(_l, r)| r.expect("ok")).collect();
        assert_eq!(items, vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]);
    }

    struct FailingReader;
    impl io::Read for FailingReader {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::Other, "boom"))
        }
    }
    impl io::BufRead for FailingReader {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Err(io::Error::new(io::ErrorKind::Other, "boom"))
        }
        fn consume(&mut self, _: usize) {}
    }

    #[test]
    fn files_from_reader_error_passthrough() {
        let mut items = files_from_reader(FailingReader, false);
        let (_line, res) = items.next().expect("one error item");
        assert!(res.is_err());
    }

    // ---- cross-source overlap (input_dir vs files_from) ----------------------------

    #[test]
    fn overlap_cross_source_input_dir_then_list() {
        let mut world = TestWorld::new();
        let tree = make_tree(&world);
        world.config.inputs.input_dirs = vec![dir_source(&tree)];
        run(&world.rt()).expect("run input dir");

        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        let sub = tree.join("sub");
        let res = handle_from_files_line(
            &world.rt(), &bar, (0, &sub), Path::new("--files-from=x"), &mut processed, &mut rec);
        assert!(matches!(res, Err(Error::Config(_))));
        rec.flush().expect("flush");
    }

    #[test]
    fn overlap_cross_source_input_dir_sub_then_list_parent() {
        let mut world = TestWorld::new();
        let tree = make_tree(&world);
        world.config.inputs.input_dirs = vec![dir_source(&tree.join("sub"))];
        run(&world.rt()).expect("run input dir");

        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        let res = handle_from_files_line(
            &world.rt(), &bar, (0, &tree), Path::new("--files-from=x"), &mut processed, &mut rec);
        assert!(matches!(res, Err(Error::Config(_))));
        rec.flush().expect("flush");
    }

    #[test]
    fn overlap_cross_source_list_then_list() {
        // Both from files_from: first registers `tree/sub`, the second `tree` overlaps it.
        let world = TestWorld::new();
        let tree = make_tree(&world);
        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        let sub = tree.join("sub");
        handle_from_files_line(
            &world.rt(), &bar, (0, &sub), Path::new("--files-from=x"), &mut processed, &mut rec)
            .expect("first line ok");
        let res = handle_from_files_line(
            &world.rt(), &bar, (0, &tree), Path::new("--files-from=x"), &mut processed, &mut rec);
        assert!(matches!(res, Err(Error::Config(_))));
        rec.flush().expect("flush");
    }

    // ---- handle_from_files_line -----------------------------------------------------

    #[test]
    fn from_line_joins_relative_to_directory() {
        let mut world = TestWorld::new();
        let tree = make_tree(&world);
        world.config.paths.directory = world.dir.path().to_path_buf();
        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        let rel = PathBuf::from("tree/a.txt");
        handle_from_files_line(
            &world.rt(), &bar, (0, &rel), Path::new("--files-from=x"), &mut processed, &mut rec)
            .expect("ok");
        rec.flush().expect("flush");
        assert!(world.db.file_id_by_abs_path(&tree.join("a.txt")).expect("lookup").is_some());
        assert_eq!(processed, 1);
    }

    #[test]
    fn from_line_overlap_errors_strict() {
        let world = TestWorld::new();
        let tree = make_tree(&world);
        add_source(&world, &tree);
        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        let sub = tree.join("sub");
        let res = handle_from_files_line(
            &world.rt(), &bar, (0, &sub), Path::new("--files-from=x"), &mut processed, &mut rec);
        match res {
            Err(Error::Config(msg)) => assert!(msg.contains("overlaps"), "{msg}"),
            other => panic!("expected Config error, got {other:?}"),
        }
        rec.flush().expect("flush");
    }

    #[test]
    fn from_line_overlap_no_strict_separation_walks_anyway() {
        let mut world = TestWorld::new();
        let tree = make_tree(&world);
        add_source(&world, &tree);
        world.config.indexing.no_strict_separation = true;
        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        let sub = tree.join("sub");
        handle_from_files_line(
            &world.rt(), &bar, (0, &sub), Path::new("--files-from=x"), &mut processed, &mut rec)
            .expect("warn and continue");
        rec.flush().expect("flush");
        // Both dir sources are registered (the overlap is walked anyway).
        let sources = world.db.list_sources(Some(true), 0, 100).expect("sources");
        assert_eq!(sources.len(), 2);
    }

    #[test]
    fn from_line_sanitize_skip_non_utf8() {
        let mut world = TestWorld::new();
        world.config.paths.directory = world.dir.path().to_path_buf();
        let mut processed = 0u64;
        let bad = non_utf8(b"bad-\xFF\xFE");
        // strict: the sanitize gate returns early (no source, no error).
        {
            let mut rec = recorder(&world);
            let bar = sub_bar(&world);
            handle_from_files_line(
                &world.rt(), &bar, (0, &bad), Path::new("--files-from=x"), &mut processed, &mut rec)
                .expect("strict skip returns ok");
            rec.flush().expect("flush");
        }
        assert_eq!(processed, 0);
        assert_eq!(world.db.count_entries().expect("count"), 0);

        // fail_fast: the sanitize failure aborts.
        world.config.process.fail_fast = true;
        {
            let mut rec = recorder(&world);
            let bar = sub_bar(&world);
            let res = handle_from_files_line(
                &world.rt(), &bar, (0, &bad), Path::new("--files-from=x"), &mut processed, &mut rec);
            assert!(res.is_err());
            rec.flush().expect("flush");
        }
    }

    // ---- handle_dir ----------------------------------------------------------------

    #[test]
    fn dir_walk_error_records_and_continues() {
        if geteuid().is_root() {
            return;
        }
        let world = TestWorld::new();
        let tree = make_tree(&world);
        fs::set_permissions(tree.join("sub"), fs::Permissions::from_mode(0)).expect("chmod 000");
        let source_id = add_source(&world, &tree);
        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        handle_dir(&world.rt(), &bar, source_id, &tree, &mut processed, &mut rec).expect("ok");
        rec.flush().expect("flush");
        // The unreadable `sub` is yielded as an entry but its contents fail to
        // read -> session error recorded; `sub` itself + `a.txt` still inventoried.
        assert!(world.db.count_errors(Some("inventory")).expect("errors") >= 1);
        assert_eq!(world.db.count_entries().expect("count"), 3);
    }

    #[test]
    fn dir_walk_error_fail_fast_aborts() {
        if geteuid().is_root() {
            return;
        }
        let mut world = TestWorld::new();
        let tree = make_tree(&world);
        fs::set_permissions(tree.join("sub"), fs::Permissions::from_mode(0)).expect("chmod 000");
        world.config.process.fail_fast = true;
        let source_id = add_source(&world, &tree);
        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        let res = handle_dir(&world.rt(), &bar, source_id, &tree, &mut processed, &mut rec);
        assert!(matches!(res, Err(Error::Config(_))));
        rec.flush().expect("flush");
    }

    #[test]
    fn dir_dereference_follows_symlink_dir() {
        let mut world = TestWorld::new();
        let tree = make_tree(&world);
        std::os::unix::fs::symlink(tree.join("sub"), tree.join("link")).expect("symlink");
        world.config.indexing.dereference = true;
        let source_id = add_source(&world, &tree);
        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        handle_dir(&world.rt(), &bar, source_id, &tree, &mut processed, &mut rec).expect("ok");
        rec.flush().expect("flush");
        // The symlinked dir is descended: `link`, `link/b.txt`, `link/c.txt` in addition
        // to the tree (tree, a.txt, sub, sub/b.txt, sub/c.txt) = 8.
        assert_eq!(world.db.count_entries().expect("count"), 8);
        assert_eq!(rows_under(&world, &tree.join("link")).len(), 3);
    }

    #[test]
    fn dir_no_dereference_keeps_link() {
        let world = TestWorld::new();
        let tree = make_tree(&world);
        std::os::unix::fs::symlink(tree.join("sub"), tree.join("link")).expect("symlink");
        let source_id = add_source(&world, &tree);
        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        handle_dir(&world.rt(), &bar, source_id, &tree, &mut processed, &mut rec).expect("ok");
        rec.flush().expect("flush");
        assert_eq!(world.db.count_entries().expect("count"), 6);
        // Only the link row itself sits under `link/` (no descent without deref).
        assert_eq!(rows_under(&world, &tree.join("link")).len(), 1);
        let link = world.db.file_id_by_abs_path(&tree.join("link")).expect("lookup").expect("link row");
        let row = world.db.get_file_by_id::<FileRecord>(link).expect("get").expect("row");
        assert_eq!(row.ftype, FileType::Symlink(LinkType::Directory));
    }

    #[test]
    fn dir_one_file_system_skips_other_dev() {
        if !Path::new("/proc/version").exists() {
            eprintln!("skipped (unsupported): /proc not available");
            return;
        }
        let mut world = TestWorld::new();
        let tree = make_tree(&world);
        std::os::unix::fs::symlink(Path::new("/proc"), tree.join("link")).expect("symlink");
        world.config.indexing.dereference = true;
        world.config.indexing.one_file_system = true;
        let source_id = add_source(&world, &tree);
        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        handle_dir(&world.rt(), &bar, source_id, &tree, &mut processed, &mut rec).expect("ok");
        rec.flush().expect("flush");
        // The cross-device subtree is skipped: nothing under /proc was inventoried.
        assert!(rows_under(&world, Path::new("/proc")).is_empty());
    }

    // ---- handle_entry_base -----------------------------------------------------------

    #[test]
    fn entry_base_preflight_adds_ref_no_restat() {
        let world = TestWorld::new();
        let tree = make_tree(&world);
        let target = tree.join("a.txt");
        insert(&world.db, target.to_str().unwrap(), 1, FileType::File);
        let file_id = world.db.file_id_by_abs_path(&target).expect("lookup").expect("row");
        assert_eq!(ref_count(&world, file_id), 0);
        let source_id = add_source(&world, &tree);
        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        handle_entry_base(&world.rt(), &bar, &target, source_id, &mut processed, &mut rec)
            .expect("ok");
        rec.flush().expect("flush");
        // Early return: no restat, no duplicate row, just a ref.
        assert_eq!(world.db.count_entries().expect("count"), 1);
        assert_eq!(ref_count(&world, file_id), 1);
        assert_eq!(processed, 0);
    }

    #[test]
    fn entry_base_sanitize_skip_non_utf8() {
        let mut world = TestWorld::new();
        let bad = non_utf8_dir(&world);
        let source_id = add_source(&world, world.dir.path());
        let mut processed = 0u64;
        // strict: sanitize gate returns early (no insert, no error).
        {
            let mut rec = recorder(&world);
            let bar = sub_bar(&world);
            handle_entry_base(&world.rt(), &bar, &bad, source_id, &mut processed, &mut rec)
                .expect("strict skip returns ok");
            rec.flush().expect("flush");
        }
        assert_eq!(processed, 0);
        assert_eq!(world.db.count_entries().expect("count"), 0);

        world.config.process.fail_fast = true;
        {
            let mut rec = recorder(&world);
            let bar = sub_bar(&world);
            let res = handle_entry_base(&world.rt(), &bar, &bad, source_id, &mut processed, &mut rec);
            assert!(res.is_err());
            rec.flush().expect("flush");
        }
    }

    // ---- handle_entry ----------------------------------------------------------------

    #[test]
    fn entry_symlink_metadata_error() {
        let world = TestWorld::new();
        let source_id = add_source(&world, world.dir.path());
        let missing = world.dir.path().join("nope.bin");
        let san = sanitize_path(false, false, &missing, "t").expect("san").expect("present");
        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        let res = handle_entry(&world.rt(), &bar, &san, source_id, &mut processed, &mut rec);
        rec.flush().expect("flush");
        assert!(matches!(res, Err(Error::FileStat(_))));
        assert_eq!(processed, 0);
        assert!(world.db.count_errors(Some("inventory")).expect("errors") >= 1);
    }

    #[test]
    fn entry_symlink_valid() {
        let world = TestWorld::new();
        let target = world.path("real.txt");
        fs::write(&target, b"x").expect("write");
        let link = world.path("link.txt");
        std::os::unix::fs::symlink(&target, &link).expect("symlink");
        let source_id = add_source(&world, world.dir.path());
        let id = run_entry(&world, &link, source_id);
        let row = world.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row");
        assert_eq!(row.ftype, FileType::Symlink(LinkType::File));
        assert_eq!(row.link_dst, Some(target));
    }

    #[test]
    fn entry_broken_symlink_dangling() {
        let world = TestWorld::new();
        let link = world.path("dangling");
        std::os::unix::fs::symlink(world.path("missing-target"), &link).expect("symlink");
        let source_id = add_source(&world, world.dir.path());
        let id = run_entry(&world, &link, source_id);
        let row = world.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row");
        // The target does not exist -> Dangling (classified, not an error).
        assert_eq!(row.ftype, FileType::Symlink(LinkType::Dangling));
        assert_eq!(row.link_dst, Some(world.path("missing-target")));
        assert!(world.db.get_records_by_file_id(id).expect("errors").is_empty());
    }

    #[test]
    fn entry_symlink_chain_eacces_enc_err() {
        if geteuid().is_root() {
            return;
        }
        let world = TestWorld::new();
        // A symlink whose chain must traverse an unreadable directory: resolve_link
        // hits EACCES -> `Symlink(Unknown)` and the failure lands in enc_err.
        let locked = world.path("locked");
        fs::create_dir(&locked).expect("mkdir");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0)).expect("chmod");
        let link = world.path("link");
        std::os::unix::fs::symlink(locked.join("secret"), &link).expect("symlink");
        let source_id = add_source(&world, world.dir.path());
        let id = run_entry(&world, &link, source_id);
        let row = world.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row");
        assert_eq!(row.ftype, FileType::Symlink(LinkType::Unknown));
        assert_eq!(world.db.get_records_by_file_id(id).expect("errors").len(), 1);
    }

    #[test]
    fn entry_char_block_device() {
        let world = TestWorld::new();
        let char_dev = world.path("cdev");
        let block_dev = world.path("bdev");
        let mk = |p: &str, kind: &str, major: &str, minor: &str| -> bool {
            std::process::Command::new("mknod")
                .args([p, kind, major, minor]).status()
                .map(|s| s.success()).unwrap_or(false)
        };
        if !mk(char_dev.to_str().unwrap(), "c", "1", "3")
            || !mk(block_dev.to_str().unwrap(), "b", "7", "0") {
            eprintln!("skipped (unsupported): mknod requires root/CAP_MKNOD");
            return;
        }
        let source_id = add_source(&world, world.dir.path());
        let c_id = run_entry(&world, &char_dev, source_id);
        let b_id = run_entry(&world, &block_dev, source_id);
        let c = world.db.get_file_by_id::<FileRecord>(c_id).expect("get").expect("row");
        let b = world.db.get_file_by_id::<FileRecord>(b_id).expect("get").expect("row");
        assert_eq!(c.ftype, FileType::CharacterDevice);
        assert_eq!(b.ftype, FileType::BlockDevice);
        assert!(c.major.is_some() && c.minor.is_some());
        assert!(b.major.is_some() && b.minor.is_some());
    }

    #[test]
    fn entry_fifo() {
        let world = TestWorld::new();
        let fifo = world.path("fifo");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o644)).expect("mkfifo");
        let source_id = add_source(&world, world.dir.path());
        let id = run_entry(&world, &fifo, source_id);
        let row = world.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row");
        assert_eq!(row.ftype, FileType::FIFO);
    }

    #[test]
    fn entry_socket() {
        let world = TestWorld::new();
        let sock = world.path("sock");
        let _listener = UnixListener::bind(&sock).expect("bind");
        let source_id = add_source(&world, world.dir.path());
        let id = run_entry(&world, &sock, source_id);
        let row = world.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row");
        assert_eq!(row.ftype, FileType::Socket);
    }

    #[test]
    fn entry_metadata_gating_xattr() {
        let probe = TestWorld::new();
        let pf = probe.path("probe.bin");
        fs::write(&pf, b"x").expect("write");
        if xattrs::symlink_set_xattr(pf.as_path(), "t", b"v").is_err() {
            eprintln!("skipped (unsupported): user xattrs");
            return;
        }
        drop(probe);

        // capture off (shared world default): None.
        let off = TestWorld::new();
        let f = off.path("x.bin");
        fs::write(&f, b"x").expect("write");
        xattrs::symlink_set_xattr(f.as_path(), "t", b"v").expect("set xattr");
        let source_id = add_source(&off, off.dir.path());
        let id = run_entry(&off, &f, source_id);
        assert_eq!(off.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row").xattrs, None);

        // capture on: Some (base64 JSON dump).
        let on = TestWorld::tuned(|c| c.capture.do_xattrs = true);
        let f2 = on.path("y.bin");
        fs::write(&f2, b"x").expect("write");
        xattrs::symlink_set_xattr(f2.as_path(), "t", b"v").expect("set xattr");
        let source_id = add_source(&on, on.dir.path());
        let id = run_entry(&on, &f2, source_id);
        assert!(on.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row").xattrs.is_some());
    }

    #[test]
    fn entry_metadata_gating_acl() {
        let probe = TestWorld::new();
        let pf = probe.path("probe.bin");
        fs::write(&pf, b"x").expect("write");
        let acl = crate::common::xattr::get_file_acl(&pf);
        if acl.is_err() {
            eprintln!("skipped (unsupported): posix acl");
            return;
        }
        drop(probe);

        let off = TestWorld::new();
        let f = off.path("a.bin");
        fs::write(&f, b"x").expect("write");
        let source_id = add_source(&off, off.dir.path());
        let id = run_entry(&off, &f, source_id);
        assert_eq!(off.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row").posix_acl, None);

        let on = TestWorld::tuned(|c| c.capture.do_posix_acl = true);
        let f2 = on.path("b.bin");
        fs::write(&f2, b"x").expect("write");
        let source_id = add_source(&on, on.dir.path());
        let id = run_entry(&on, &f2, source_id);
        assert!(on.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row").posix_acl.is_some());
    }

    #[test]
    fn entry_metadata_gating_selinux() {
        let probe = TestWorld::new();
        let pf = probe.path("probe.bin");
        fs::write(&pf, b"x").expect("write");
        if crate::common::xattr::get_file_selinux_data(&pf).is_err() {
            eprintln!("skipped (unsupported): selinux");
            return;
        }
        drop(probe);

        let off = TestWorld::new();
        let f = off.path("s.bin");
        fs::write(&f, b"x").expect("write");
        let source_id = add_source(&off, off.dir.path());
        let id = run_entry(&off, &f, source_id);
        assert_eq!(off.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row").selinux_ctx, None);

        let on = TestWorld::tuned(|c| c.capture.do_selinux = true);
        let f2 = on.path("t.bin");
        fs::write(&f2, b"x").expect("write");
        let source_id = add_source(&on, on.dir.path());
        let id = run_entry(&on, &f2, source_id);
        assert!(on.db.get_file_by_id::<FileRecord>(id).expect("get").expect("row").selinux_ctx.is_some());
    }

    #[test]
    fn set_hardlink_canonicals_flags_min_id() {
        use std::os::unix::fs::MetadataExt;
        let world = TestWorld::new();
        let a = world.path("a.bin");
        let b = world.path("b.bin");
        fs::write(&a, b"data").expect("write a");
        fs::hard_link(&a, &b).expect("hard link");
        let md = fs::metadata(&a).expect("meta");
        let (dev, ino) = (md.dev(), md.ino());
        let a_id = insert(&world.db, a.to_str().unwrap(), 4, FileType::File);
        let b_id = insert(&world.db, b.to_str().unwrap(), 4, FileType::File);
        // The insert helper leaves (dev, inode) unset; pin the real pair.
        world.db.with_transaction(|conn| {
            conn.execute(
                "UPDATE files SET dev = :dev, inode = :ino",
                rusqlite::named_params! { ":dev": dev as i64, ":ino": ino as i64 },
            ).expect("set dev ino");
            Ok(())
        }).expect("set dev ino tx");
        assert!(a_id.0 < b_id.0);

        let n = world.db.set_hardlink_canonicals().expect("set canonicals");
        assert_eq!(n, 1);
        // The lowest id of the (dev, inode) group is the canonical.
        assert!(world.flag(a_id, FileFlag::FileHardlinkCanonical));
        assert!(!world.flag(b_id, FileFlag::FileHardlinkCanonical));
    }

    // ---- debug_assert battery (catch_unwind) -----------------------------------------

    #[test]
    fn debug_run_no_inputs() {
        let world = TestWorld::new();
        let res = catch_unwind(AssertUnwindSafe(|| { let _ = run(&world.rt()); }));
        assert!(res.is_err(), "run() without inputs must trip the PRECONDITION assert");
    }

    #[test]
    fn debug_run_input_not_dir() {
        let mut world = TestWorld::new();
        let f = world.path("f.bin");
        fs::write(&f, b"x").expect("write");
        world.config.inputs.input_dirs = vec![dir_source(&f)];
        let res = catch_unwind(AssertUnwindSafe(|| { let _ = run(&world.rt()); }));
        assert!(res.is_err(), "input dir must be a directory");
    }

    #[test]
    fn debug_entry_base_relative() {
        let world = TestWorld::new();
        let source_id = add_source(&world, world.dir.path());
        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        let res = catch_unwind(AssertUnwindSafe(|| {
            let _ = handle_entry_base(&world.rt(), &bar, Path::new("rel"), source_id, &mut processed, &mut rec);
        }));
        assert!(res.is_err(), "handle_entry_base requires absolute paths");
    }

    #[test]
    fn debug_entry_relative_san() {
        let world = TestWorld::new();
        let source_id = add_source(&world, world.dir.path());
        let san = sanitize_path(false, false, Path::new("rel"), "t").expect("san").expect("present");
        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        let res = catch_unwind(AssertUnwindSafe(|| {
            let _ = handle_entry(&world.rt(), &bar, &san, source_id, &mut processed, &mut rec);
        }));
        assert!(res.is_err(), "handle_entry requires absolute paths");
    }

    #[test]
    fn debug_from_line_relative_empty_directory() {
        let world = TestWorld::new();
        let mut processed = 0u64;
        let mut rec = recorder(&world);
        let bar = sub_bar(&world);
        let res = catch_unwind(AssertUnwindSafe(|| {
            let _ = handle_from_files_line(
                &world.rt(), &bar, (0, PathBuf::from("rel")), Path::new("--files-from=x"),
                &mut processed, &mut rec);
        }));
        assert!(res.is_err(), "a relative line with an empty directory must trip the assert");
    }
}
