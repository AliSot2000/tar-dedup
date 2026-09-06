use walkdir::WalkDir;

use crate::common::files::original_extension;
#[cfg(windows)]
use crate::common::files::get_file_times;
use crate::common::xattr::{get_file_acl, get_file_selinux_data, get_file_xattr};
use crate::config::ArchiveConfig;
use crate::db::Database;
use crate::db::flags::{SourceFlag, SourceFlags};
use crate::db::types::{FileType, LinkType, NewFileRecord};
use crate::error::{Error, FileStatError, FileStatResult, Result};
use crate::progress::CountProgress;
use crate::shutdown::Shutdown;
use chrono::{DateTime, Utc};
use path_clean::PathClean;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::{fs, io};

pub fn run(config: &ArchiveConfig, db: &Database, shutdown: &Shutdown) -> Result<()> {
    // TODO Better errors.
    // TODO on restart - delete the db and start from the beginning
    tracing::info!("Inventory pass cannot be gracefully interrupted. \
                    If force aborted, inventory needs to be run again to ensure consistent \
                    snapshot of filesystem.");
    let mut processed = 0u64;
    let progress = CountProgress::new("inventory");

    // Handle input directories
    for (index, input_dir) in config.inputs.input_dirs.iter().enumerate() {
        shutdown.check_in_flight()?;

        tracing::info!(root = %input_dir.absolute_path.display(), "inventory pass");

        // Sanity checks
        debug_assert!(input_dir.absolute_path.is_dir(),
                      "Input Dir must contain valid directories");
        debug_assert!(input_dir.absolute_path.is_absolute(),
                      "Input Dir must be absolute");
        debug_assert!(input_dir.absolute_path.clean() == input_dir.absolute_path,
                      "Path should be minimal");

        let source_id = db.add_get_source(
            &input_dir.absolute_path,
            "--input-dir",
            Some(index as u64),
            Some(&input_dir.original_path),
            SourceFlags::default().with(SourceFlag::IsDirectory, true),
        )?;
        handle_dir(&config, &db, &shutdown, source_id, &input_dir.absolute_path,
                   &mut processed, &progress)?;
    }

    // Handle from-files
    for files_file in config.inputs.files_from.iter() {
        if files_file == "-" {
            let br = BufReader::new(io::stdin());
            for element in files_from_reader(br, config.inputs.files_from_null) {
                let (line, result) = element;
                let path = match result {
                    Ok(path_vec) => path_vec,
                    Err(e) => return Err(Error::io("-", e)),
                };

                handle_from_files_line((line, &path), &files_file, &config, &db, &shutdown,
                                       &mut processed, &progress)?
            }
            continue;
        }
        // Sanity check
        debug_assert!(files_file.is_file(),
                      "Input Dir must contain valid directories");
        debug_assert!(files_file.is_absolute(),
                      "Input Dir must be absolute");
        debug_assert!(&files_file.clean() == files_file,
                      "Path should be minimal");

        let file = fs::read(files_file)
            .map_err(|e| Error::io(files_file, e))?;

        for element in files_from_records(&file, config.inputs.files_from_null) {
            handle_from_files_line(element, &files_file, &config, &db, &shutdown, &mut processed,
                                   &progress)?
        }
    }
    // Set hardlink canonicals if and only if, we want to collapse the hardlinks and
    if !config.indexing.no_hardlink_detection {
        let rows = db.set_hardlink_canonicals()?;
        tracing::info!("Updated {rows} of hardlink groups to have one canonical");
    }

    db.resolve_numeric_ids()?;
    progress.finish("inventory complete");
    let missing_dev_ino_count = db.count_missing_dev_inode()?;
    if missing_dev_ino_count > 0 {
        tracing::warn!("Encountered {missing_dev_ino_count} files without dev or inode information.\
         Those files won't be captured by hardlink detection");
    }
    // TODO: Add inspect with query for missing files.
    tracing::info!(
        entries_processed = processed,
        total_unique_entries = db.count_entries()?,
        "inventory indexed");
    Ok(())
}

/// Process a single line from the --from-files argument
fn handle_from_files_line(
    element: (usize, &[u8]),
    from_files_path: &Path,
    config: &ArchiveConfig,
    db: &Database,
    shutdown: &Shutdown,
    processed: &mut u64,
    progress: &CountProgress,
) -> Result<()> {
    let (line, ff) = element;
    let fpath_os = os_str_from_bytes(ff);
    let fpath = Path::new(&fpath_os); // TODO force utf8
    let from_files_disp_path = from_files_path.display();

    let abs_path = if fpath.is_absolute() {
        fpath.to_path_buf().clean()
    } else {
        config.paths.directory.join(fpath).clean()
    };
    debug_assert!(abs_path.is_absolute(), "Path must be absolute now");

    if abs_path.is_dir() {
        if let Some((_, existing)) = db.find_overlapping_source(
            &abs_path, config.indexing.no_recursion)? {
            if !config.indexing.no_strict_separation {
                return Err(Error::Config(format!(
                    "input directory `{}` overlaps `{}`; use `--no-strict-separation` to walk anyway",
                    abs_path.display(),
                    existing.display()
                )));
            }
        }
    }

    let source_id = db.add_get_source(
        &abs_path,
        &format!("--files-from={from_files_disp_path}"),
        Some(line as u64),
        Some(&fpath.clean()),
        SourceFlags::default().with(SourceFlag::IsDirectory, abs_path.is_dir()),
    )?;
    handle_dir(&config, &db, &shutdown, source_id, &abs_path,
               processed, &progress)?;

    Ok(())
}

/// Handle a single dir by walking the directory or the directory tree
/// PRECONDITION:
/// - Directory exists
/// - Path is minimal
/// - Path is directory.
/// - Path is on the same file system if called recursively
pub fn handle_dir(
    config: &ArchiveConfig,
    db: &Database,
    shutdown: &Shutdown,
    source_id: i64,
    start_dir: &Path,
    processed: &mut u64,
    progress: &CountProgress)
    -> Result<()> {

    let mut iter = WalkDir::new(&start_dir)
        .follow_links(config.indexing.dereference)
        .follow_root_links(true)// INFO: Custom handling by us
        .same_file_system(config.indexing.one_file_system)
        .min_depth(0)
        .max_depth(if config.indexing.no_recursion { 1 } else { usize::MAX })
        .contents_first(false)
        .into_iter();

    while let Some(element) = iter.next() {
        shutdown.check_in_flight()?;
        let entry = match element {
            Err(e) => {
                tracing::error!("Failed to access element with error: {e}"); // TODO fail fast
                continue;
            }
            Ok(entry) => entry,
        };
        handle_entry(&entry.path(), source_id, &config, &db, &progress, processed)?;
    }
    Ok(())
}

pub fn handle_entry_base(path: &Path,
                         source_id: i64,
                         config: &ArchiveConfig,
                         db: &Database,
                         progress: &CountProgress,
                         processed: &mut u64)
                         -> Result<()> {
    debug_assert!(path.is_absolute(), "Expected Absolute paths only.");

    // Preflight: already inventoried — attach this source without restatting.
    if let Some(file_id) = db.file_id_by_abs_path(path)? {
        db.add_ref(source_id, file_id)?;
        return Ok(());
    }

    handle_entry(&path, source_id, &config, &db, &progress, processed)
}

#[cfg(unix)]
pub fn handle_entry(
    path: &Path,
    source_id: i64,
    config: &ArchiveConfig,
    db: &Database,
    progress: &CountProgress,
    processed: &mut u64)
    -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let mut enc_err = Vec::new();
    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            // TODO store error in db,
            return Err(Error::io(path, e))
        }
    };

    let mtime_s = meta.mtime();
    let mtime_nsec = meta.mtime_nsec();
    debug_assert!((0..1_000_000_000).contains(&mtime_nsec));
    // TODO Error on NONE
    let mtime: Option<DateTime<Utc>> = DateTime::from_timestamp(mtime_s, mtime_nsec as u32);
    if mtime.is_none(){
        tracing::warn!("File {} has Implausible Timestamp.  {}s, {}nsec",
            path.display(), mtime_s, mtime_nsec);
    }

    let atime_s = meta.atime();
    let atime_nsec = meta.atime_nsec();
    debug_assert!((0..1_000_000_000).contains(&atime_nsec));
    // TODO Error on NONE
    let atime: Option<DateTime<Utc>> = DateTime::from_timestamp(atime_s, atime_nsec as u32);
    if atime.is_none(){
        tracing::warn!("File {} has Implausible Timestamp.  {}s, {}nsec",
            path.display(), atime_s, atime_nsec);
    }

    let ctime_s = meta.mtime();
    let ctime_nsec = meta.mtime_nsec();
    debug_assert!((0..1_000_000_000).contains(&ctime_nsec));
    // TODO Error on NONE
    let ctime: Option<DateTime<Utc>> = DateTime::from_timestamp(ctime_s, ctime_nsec as u32);
    if ctime.is_none(){
        tracing::warn!("File {} has Implausible Timestamp.  {}s, {}nsec",
            path.display(), ctime_s, ctime_nsec);
    }

    let mode = meta.mode();
    let uid = meta.uid();
    let gid = meta.gid();
    let dev = meta.dev();
    let ino = meta.ino();

    let ftype = match determine_file_type(&meta, &path) {
        Ok(t) => t,
        Err((t, e)) => {
            enc_err.push(e);
            t
        }
    };

    let (link_dst, major, minor) = match ftype {
        FileType::Symlink(_) => (strip_transpose(path, fs::read_link(path), &mut enc_err),
                                 None,
                                 None),
        FileType::CharacterDevice | FileType::BlockDevice => {
            let (maj, min) = get_file_rdev_parts(&meta);
            (None, maj, min)
        }
        FileType::Unknown => {
            tracing::error!("{} could not be classified into a valid file type.", path.display());
            (None, None, None)
        },
        _ => (None, None, None)
    };

    // Optional data
    let xattrs = if config.capture.do_xattrs {
        match get_file_xattr(path) {
            Err(e) => { enc_err.push(e); None},
            Ok(md) => Some(md),
        }
    } else { None };
    let posix_acl = if config.capture.do_posix_acl {
        match get_file_acl(path) {
            Err(e) => { enc_err.push(e); None},
            Ok(md) => Some(md),
        }
    } else { None };
    let selinux_ctx = if config.capture.do_selinux {
        match get_file_selinux_data(path) {
            Err(e) => { enc_err.push(e); None},
            Ok(md) => Some(md),
        }
    } else { None };

    if db.insert_file_and_ref(source_id, &NewFileRecord {
        abs_path: path.clean().to_path_buf(),
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
        link_dst: link_dst.clone(),
        device_id: Some(dev),
        inode_id: Some(ino),
        major,
        minor,
    })? {
        *processed += 1;
        progress.inc(1);
        // TODO deal with the error vec!
    }
    Ok(())
}

/// Handle a single dir entry.
#[cfg(windows)]
pub fn handle_entry(
    path: &Path,
    source_id: i64,
    _config: &ArchiveConfig,
    db: &Database,
    progress: &CountProgress,
    processed: &mut u64)
    -> Result<()> {
    let mut enc_err = Vec::new();

    let meta = match fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) => {
            // TODO store error in db,
            return Err(Error::io(path, e))
        }
    };

    // Extract times, retaining the errors.
    let times = get_file_times(&meta);
    let mtime = strip_transpose(path, times.0, &mut enc_err);
    let atime = strip_transpose(path, times.1, &mut enc_err);
    let ctime = strip_transpose(path, times.2, &mut enc_err);
    let uid = None;
    let gid = None;
    let ftype = match determine_file_type(&meta, &path) {
        Ok(t) => t,
        Err((t, e)) => {
            enc_err.push(e);
            t
        }
    };

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

    if db.insert_file_and_ref(source_id, &NewFileRecord {
        abs_path: path.clean().to_path_buf(),
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
        device_id: dev,
        inode_id: ino,
        major,
        minor,
    })? {
        *processed += 1;
        progress.inc(1);
        // TODO deal with the error vec!
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
        Err(e) => { errors.push(FileStatError::Io {
            path: path.to_path_buf(),
            source: e});
            None},
        Ok(dt_utc) => Some(dt_utc),
    }
}

/// Split an arbitrary file into slices which are
/// - delimited by either `\0` or `\n`
/// - not empty (blank lines / trailing separator)
fn files_from_records(buf: &[u8], null: bool) -> impl Iterator<Item = (usize, &[u8])> {
    let sep = if null { b'\0' } else { b'\n' };
    buf.split(move |&b| b == sep)
        .enumerate()
        .map(|(line, rec)| (line, rec.strip_suffix(b"\r").unwrap_or(rec)))
        .filter(|(_line, rec)| !rec.is_empty())
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
    }).enumerate()
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
    meta.file_index().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "file index is not available")
    })
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


/// Function attempts to figure out what a given soft link (chain) is pointing to.
/// If a link is a part of a link cycle, a `Cycle` is emitted
/// If a link returns a NotFound Error, `Dangling` is returned
/// If a link target cannot be resolved (any other error e.g. permission error), `Unknown` is return
fn resolve_link(e: &Path) -> FileStatResult<LinkType> {
    let mut visited = HashSet::new();
    let mut current = e.to_path_buf();
    debug_assert!(current.is_symlink(), "INVARIANT: Non-Link DirEntry supplied");
    debug_assert!(current.is_absolute(), "INVARIANT: Non-Absolute Path supplied");

    loop {
        // Cycle prevention.
        if !visited.insert(current.clone()) {
            return Ok(LinkType::Cycle); // cycle detected
        }

        let ft = match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                // Deal with next step resolution.
                let target = fs::read_link(&current);
                match target {
                    Ok(pb) => {
                        current = resolve_relative(&current, &pb.as_path());
                        continue;
                    }
                    Err(e) => {
                        let fmt_path = current.as_os_str().to_string_lossy();
                        tracing::warn!("Resolving {fmt_path} resulted an error: {e}");
                        return Ok(LinkType::Unknown);
                    }
                }
            }
            Ok(meta) => meta.file_type(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LinkType::Dangling),
            Err(g) => return Err(FileStatError::Io {path: current.to_path_buf(), source: g}),
        };

        // Match valid target
        return Ok(classify_link_target(ft));
    }
}

/// Classify the resolved, non-symlink target of a link chain.
#[cfg(unix)]
fn classify_link_target(ft: fs::FileType) -> LinkType {
    use std::os::unix::fs::FileTypeExt;
    if ft.is_file() {
        LinkType::File
    } else if ft.is_dir() {
        LinkType::Directory
    } else if ft.is_fifo() {
        LinkType::FIFO
    } else if ft.is_char_device() {
        LinkType::CharacterDevice
    } else if ft.is_block_device() {
        LinkType::BlockDevice
    } else if ft.is_socket() {
        LinkType::Socket
    } else {
        LinkType::Unknown
    }
}

/// Windows link targets can only be files or directories (the symlink dir/file
/// distinction comes from `is_symlink_dir` in the caller's `file_type()`).
#[cfg(windows)]
fn classify_link_target(ft: std::fs::FileType) -> LinkType {
    if ft.is_file() {
        LinkType::File
    } else if ft.is_dir() {
        LinkType::Directory
    } else {
        LinkType::Unknown
    }
}

/// Handle solving for new linking target.`link_path` refers to the current location of the source
/// of the symlink and `target` to the resolved target given the current symlink
fn resolve_relative(link_path: &Path, target: &Path) -> PathBuf {
    debug_assert!(link_path.is_absolute(), "link_path must be absolute");

    let joined = if target.is_absolute() {
        target.to_path_buf()
    } else {
        link_path
            .parent()
            .expect("absolute path must have a parent")
            .join(target)
    };

    joined.clean()
}

#[cfg(unix)]
fn determine_file_type(md: &fs::Metadata, path: &Path)
    -> std::result::Result<FileType, (FileType, FileStatError)> {
    use std::os::unix::fs::FileTypeExt;

    // walkdir::DirEntry::file_type() is infallible.
    let ft = md.file_type();
    if ft.is_file() {
        Ok(FileType::File)
    } else if ft.is_dir() {
        Ok(FileType::Directory)
    } else if ft.is_fifo() {
        Ok(FileType::FIFO)
    } else if ft.is_block_device() {
        Ok(FileType::BlockDevice)
    } else if ft.is_char_device() {
        Ok(FileType::CharacterDevice)
    } else if ft.is_symlink() {
        let err_symlink = resolve_link(path);
        match err_symlink {
            Ok(lt) => Ok(FileType::Symlink(lt)),
            Err(e) => Err((FileType::Symlink(LinkType::Unknown), e)),
        }
    } else if ft.is_socket() {
        Ok(FileType::Socket)
    } else {
        Ok(FileType::Unknown)
    }
}

#[cfg(windows)]
fn determine_file_type(md: &fs::Metadata, path: &Path)
    -> std::result::Result<FileType, (FileType, FileStatError)> {
    // walkdir::DirEntry::file_type() is infallible.
    let ft = md.file_type();

    if ft.is_file() {
        Ok(FileType::File)
    } else if ft.is_dir() {
        Ok(FileType::Directory)
    } else if ft.is_symlink() {
        // Resolve the chain: yields the eventual target type plus Dangling /
        // Cycle / Unknown for broken or unresolvable links.
        match resolve_link(path) {
            Ok(lt) => Ok(FileType::Symlink(lt)),
            Err(e) => Err((FileType::Symlink(LinkType::Unknown), e)),
        }
    } else {
        Ok(FileType::Unknown)
    }
}