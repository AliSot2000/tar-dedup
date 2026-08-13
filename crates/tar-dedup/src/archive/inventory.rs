#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;

use walkdir::WalkDir;

use crate::common::files::device_num_lstat;
use crate::common::files::{get_file_times, original_extension};
use crate::common::xattr::{get_file_acl, get_file_selinux_data, get_file_xattr};
use crate::config::Config;
use crate::db::Database;
use crate::db::types::{FileType, LinkType, NewFileRecord};
use crate::error::{Error, FileStatError, Result};
use crate::progress::CountProgress;
use crate::shutdown::Shutdown;
use path_clean::PathClean;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::{fs, io};

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    // TODO Better errors.
    let mut processed = 0u64;
    let progress = CountProgress::new("inventory");

    let mut path_cycle_detect: Vec<PathBuf> = Vec::new();

    // Handle input directories
    for input_dir in config.input_dirs.iter() {
        shutdown.check_between_files()?;
        path_cycle_detect.clear();

        tracing::info!(root = %input_dir.absolute_path.display(), "inventory pass");

        // Sanity checks
        debug_assert!(input_dir.absolute_path.is_dir(),
                      "Input Dir must contain valid directories");
        debug_assert!(input_dir.absolute_path.is_absolute(),
                      "Input Dir must be absolute");
        debug_assert!(input_dir.absolute_path.clean() == input_dir.absolute_path,
                      "Path should be minimal");

        let source_id = db.add_get_source(
            &input_dir.absolute_path, "--input-dir", None)?;
        handle_dir(&config, &db, &shutdown, source_id, &input_dir.absolute_path,
                   &mut processed, &progress, &mut path_cycle_detect)?;
    }

    // Handle from-files
    for files_file in config.files_from.iter() {
        if files_file == "-" {
            let br = BufReader::new(io::stdin());
            for element in files_from_reader(br, config.files_from_null){
                let (line, result) = element;
                let path = match result {
                    Ok(path_vec) => path_vec,
                    Err(e) => return Err(Error::io("-", e)),
                };

                path_cycle_detect.clear();
                handle_from_files_line((line, &path), &files_file, &config, &db, &shutdown,
                                       &mut processed, &progress, &mut path_cycle_detect)?
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

        for element in files_from_records(&file, config.files_from_null) {
            path_cycle_detect.clear();
            handle_from_files_line(element, &files_file, &config, &db, &shutdown, &mut processed,
                                   &progress, &mut path_cycle_detect)?
        }
    }

    db.resolve_numeric_ids()?;
    progress.finish("inventory complete");
    tracing::info!(processed, total = db.count_files()?, "inventory indexed");
    Ok(())
}

/// Process a single line from the --from-files argument
fn handle_from_files_line(
    element: (usize, &[u8]),
    from_files_path: &Path,
    config: &Config,
    db: &Database,
    shutdown: &Shutdown,
    processed: &mut u64,
    progress: &CountProgress,
    path_cycle_detect: &mut Vec<PathBuf>
) -> Result<()> {
    let (line, ff) = element;
    let fpath = Path::new(OsStr::from_bytes(ff));
    let from_files_disp_path = from_files_path.display();

    let abs_path = if fpath.is_absolute() {
        fpath.to_path_buf().clean()
    } else {
        config.directory.join(fpath).clean()
    };
    debug_assert!(abs_path.is_absolute(), "Path must be absolute now");

    let source_id = db.add_get_source(
        &abs_path, &format!("--from-files={from_files_disp_path}"), Some(line as u64))?;
    handle_dir(&config, &db, &shutdown, source_id, &fpath,
               processed, &progress, path_cycle_detect)?;

    Ok(())
}

/// Handle a single dir by walking the directory or the directory tree
/// PRECONDITION:
/// - Directory exists
/// - Path is minimal
/// - Path is directory.
/// - Path is on the same file system if called recursively
pub fn handle_dir(
    config: &Config,
    db: &Database,
    shutdown: &Shutdown,
    source_id: i64,
    start_dir: &Path,
    processed: &mut u64,
    progress: &CountProgress,
    cycle_detector: &mut Vec<PathBuf>) -> Result<()> {

    let pb_start_dir = start_dir.to_path_buf();
    if cycle_detector.contains(&pb_start_dir) {
        // TODO Print chain
        return Ok(())
    }
    cycle_detector.push(pb_start_dir);

    let root_device = device_num(&start_dir).map_err(
        |e| Error::io(&start_dir, e)
    )?;

    let mut iter = WalkDir::new(&start_dir)
        .follow_links(false)
        .same_file_system(config.one_file_system)
        .min_depth(if config.no_recursion { 1 } else { 0 })
        .max_depth(if config.no_recursion { 1 } else { usize::MAX })
        .contents_first(false)
        .into_iter();

    while let Some(element) = iter.next() {
        shutdown.check_between_files()?;
        let entry = match element {
            Err(e) => {
                tracing::error!("Failed to access element with error: {e}"); // TODO fail fast
                continue;
            }
            Ok(entry) => entry,
        };
        let res = handle_entry(
            &entry.path(), source_id, &config, &db, &progress, processed, root_device,
            &shutdown, cycle_detector)?;

        // Entire directory scanned and we are in dir first mode
        if res {
            iter.skip_current_dir();
        }
    }
    let _ =cycle_detector.pop();
    Ok(())
}

/// Handle a single dir entry.
pub fn handle_entry(
    path: &Path,
    source_id: i64,
    config: & Config,
    db: &Database,
    progress: &CountProgress,
    processed: &mut u64,
    root_device: u64,
    shutdown: &Shutdown,
    cycle_detect: &mut Vec<PathBuf>) -> Result<bool> {
    let mut enc_err = Vec::new();

    debug_assert!(path.is_absolute(), "Expected Absolute paths only.");
    let meta = fs::metadata(path)
        .map_err(|e| crate::error::Error::io(path, e))?;
    let mode = file_mode(&meta);

    // Extract times, retaining the errors.
    let times = get_file_times(&meta);
    let mtime = strip_transpose(path, times.0, &mut enc_err);
    let atime = strip_transpose(path, times.1, &mut enc_err);
    let ctime = strip_transpose(path, times.2, &mut enc_err);
    let uid = strip_transpose(path, file_uid(&path), &mut enc_err);
    let gid = strip_transpose(path, file_gid(&path), &mut enc_err);
    let ftype = strip_transpose(path, determine_file_type(&path), &mut enc_err);
    let dev = strip_transpose(path, get_file_dev(&meta), &mut enc_err);
    let ino = strip_transpose(path, get_file_ino(&meta), &mut enc_err);

    let link_dst: Option<PathBuf> = if matches!(ftype, Some(FileType::Symlink(_))) {
        strip_transpose(path, fs::read_link(path), &mut enc_err)
    } else {
        None
    };

    // Optional data
    let xattrs = if config.do_xattrs {
        match get_file_xattr(path) {
            Err(e) => { enc_err.push(e); None},
            Ok(md) => Some(md),
        }
    } else { None };
    let posix_acl = if config.do_posix_acl {
        match get_file_acl(path) {
            Err(e) => { enc_err.push(e); None},
            Ok(md) => Some(md),
        }
    } else { None };
    let selinux_ctx = if config.do_selinux {
        match get_file_selinux_data(path) {
            Err(e) => { enc_err.push(e); None},
            Ok(md) => Some(md),
        }
    } else { None };

    if db.insert_file(&NewFileRecord {
        abs_path: path.to_path_buf(),
        ext: original_extension(&path),
        size: meta.len(),
        mtime,
        atime,
        ctime,
        uid,
        gid,
        ftype,
        mode: Some(mode),
        xattrs,
        posix_acl,
        selinux_ctx,
        link_dst: link_dst.clone(),
        device_id: dev,
        inode_id: ino,
        source_id,
    })? {
        *processed += 1;
        progress.inc(1);
        // TODO deal with the error vec!
    } else {
        // Handle case of already seen directory.
        if let Some(ft) = ftype {
            if ft == FileType::Directory {
                return Ok(true);
            }
        }

        // In any other case, (even symlink) we have seen this before,
        // so we can safely assume it's done.
        return Ok(false);
    }

    // Deal with soft links and potentially start a new dir scan.
    if let Some(ld) = link_dst && config.dereference {
        debug_assert!(matches!(ftype, Some(FileType::Symlink(_))),
                      "Link Resolution may only happen for elements of type Symlink(_)");
        walk_link(&config, &db, &shutdown, &ld, &path, root_device, source_id, &progress,
                  processed, cycle_detect)?;
    }

    Ok(false)
}

/// Handle the case when we want to escape from our current tree into a new tree segment as a
/// consequence of dereferencing a link.
/// PRECONDITION: path is link
fn walk_link(config: &Config, db: &Database, shutdown: &Shutdown,
             link_dst: &Path, link_path: &Path, root_device: u64, source_id: i64,
             progress: &CountProgress, processed: &mut u64, cycle_detect: &mut Vec<PathBuf>)
    -> Result<()> {
    let parent = link_path.parent().expect("File must be inside a directory.");
    let abs_dst = parent.join(link_dst).clean();

    // PRECONDITION: Dereference
    if !abs_dst.exists() { return Ok(()); }

    // PRECONDITION: Dereference, Path Exists
    let target_device_number = device_num(&abs_dst)
        .map_err(|e| Error::io(&abs_dst, e))?;
    if target_device_number != root_device && config.one_file_system{ return Ok(()); }

    // PRECONDITION: Dereference, Path exists and is on same file system
    if !abs_dst.is_dir() {
        // INFO: Symlink found -> element added to db.
        let _ = handle_entry(
            &abs_dst, source_id, &config, &db, &progress, processed, root_device,
            &shutdown, cycle_detect
        )?;
    } else {
        // PRECONDITION: Dereference, Path exists, is on same file system, is directory.
        //  Start Dir walk again
        debug_assert!(abs_dst.is_dir(),
                      "Previously handled all non-dir entries. Now should be dir.");
        let _ = handle_dir(&config, &db, &shutdown, source_id, &abs_dst, processed,
                           &progress, cycle_detect)?;
    }
    Ok(())
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

#[cfg(unix)]
fn file_uid(path: &Path) -> io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    Ok(fs::metadata(path)?.uid())
}

#[cfg(not(unix))]
fn file_uid(_path: &Path) -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file uid is not available on this platform",
    ))
}

#[cfg(unix)]
fn file_gid(path: &Path) -> io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    Ok(fs::metadata(path)?.gid())
}

#[cfg(not(unix))]
fn file_gid(_path: &Path) -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file uid is not available on this platform",
    ))
}

#[cfg(unix)]
fn file_mode(meta: &fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode()
}

#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

#[cfg(unix)]
fn get_file_dev(meta: &fs::Metadata) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(meta.dev())
}

#[cfg(not(unix))]
fn get_file_def() -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file dev is not available on this platform",
    ))
}

#[cfg(unix)]
fn get_file_ino(meta: &fs::Metadata) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    Ok(meta.ino())
}

#[cfg(not(unix))]
fn get_file_def() -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file ino is not available on this platform",
    ))
}

/// Function attempts to figure out what a given soft link (chain) is pointing to.
/// If a link is a part of a link cycle, a `Cycle` is emitted
/// If a link returns a NotFound Error, `Dangling` is returned
/// If a link target cannot be resolved (any other error e.g. permission error), `Unknown` is return
#[cfg(unix)]
fn resolve_link(e: &Path) -> io::Result<LinkType> {
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
            Err(g) => return Err(g),
        };

        // Match valid target
        return if ft.is_file() {
            Ok(LinkType::File)
        } else if ft.is_dir() {
            Ok(LinkType::Directory)
        } else if ft.is_fifo() {
            Ok(LinkType::FIFO)
        } else if ft.is_char_device() {
            Ok(LinkType::CharacterDevice)
        } else if ft.is_block_device() {
            Ok(LinkType::BlockDevice)
        } else if ft.is_socket() {
            Ok(LinkType::Socket)
        } else {
            Ok(LinkType::Unknown)
        }
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
fn determine_file_type(path: &Path) -> io::Result<FileType> {
    // walkdir::DirEntry::file_type() is infallible.
    let ft = path.metadata()?.file_type();
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
        Ok(FileType::Symlink(resolve_link(&path)?))
    } else {
        Ok(FileType::Unknown)
    }
}

#[cfg(windows)]
fn determine_file_type(e: std::fs::DirEntry) -> io::Result<FileType> {
    use std::os::windows::fs::FileTypeExt;

    let ft = match e.file_type() {
        Ok(o) => o,
        Err(e) => {
            println!("Failed to resolve file type {}", e);
            return Ok(FileType::Unknown);
        }
    };
    // Iterate through all possible file types
    if ft.is_file() {
        Ok(FileType::File)
    } else if ft.is_dir() {
        Ok(FileType::Directory)
    } else if ft.is_symlink_dir() {
        Ok(FileType::Symlink(LinkType::Directory))
    } else if ft.is_symlink_file() {
        Ok(FileType::Symlink(LinkType::File))
    } else {
        Ok(FileType::Unknown)
    }
}