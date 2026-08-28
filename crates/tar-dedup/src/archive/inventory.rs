#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;

use walkdir::WalkDir;

use crate::common::files::{get_file_times, original_extension};
use crate::common::xattr::{get_file_acl, get_file_selinux_data, get_file_xattr};
use crate::config::Config;
use crate::db::flags::{SourceFlag, SourceFlags};
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
    // TODO on restart - delete the db and start from the beginning
    tracing::info!("Inventory pass cannot be gracefully interrupted. \
                    If force aborted, inventory needs to be run again to ensure consistent \
                    snapshot of filesystem.");
    let mut processed = 0u64;
    let progress = CountProgress::new("inventory");

    // Handle input directories
    for (index, input_dir) in config.input_dirs.iter().enumerate() {
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
    for files_file in config.files_from.iter() {
        if files_file == "-" {
            let br = BufReader::new(io::stdin());
            for element in files_from_reader(br, config.files_from_null){
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

        for element in files_from_records(&file, config.files_from_null) {
            handle_from_files_line(element, &files_file, &config, &db, &shutdown, &mut processed,
                                   &progress)?
        }
    }
    // Set hardlink canonicals if and only if, we want to collapse the hardlinks and
    if !config.no_hardlink_detection {
        let rows = db.set_hardlink_canonicals()?;
        tracing::info!("Updated {rows} of hardlink groups to have one canonical");
    }

    db.resolve_numeric_ids()?;
    progress.finish("inventory complete");
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
    config: &Config,
    db: &Database,
    shutdown: &Shutdown,
    processed: &mut u64,
    progress: &CountProgress,
) -> Result<()> {
    let (line, ff) = element;
    let fpath = Path::new(OsStr::from_bytes(ff)); // TODO force utf8
    let from_files_disp_path = from_files_path.display();

    let abs_path = if fpath.is_absolute() {
        fpath.to_path_buf().clean()
    } else {
        config.directory.join(fpath).clean()
    };
    debug_assert!(abs_path.is_absolute(), "Path must be absolute now");

    if abs_path.is_dir() {
        if let Some((_, existing)) = db.find_overlapping_source(&abs_path, config.no_recursion)? {
            if !config.no_strict_separation {
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
    config: &Config,
    db: &Database,
    shutdown: &Shutdown,
    source_id: i64,
    start_dir: &Path,
    processed: &mut u64,
    progress: &CountProgress)
    -> Result<()> {

    let mut iter = WalkDir::new(&start_dir)
        .follow_links(config.dereference)
        .follow_root_links(true)// INFO: Custom handling by us
        .same_file_system(config.one_file_system)
        .min_depth(0)
        .max_depth(if config.no_recursion { 1 } else { usize::MAX })
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

/// Handle a single dir entry.
pub fn handle_entry(
    path: &Path,
    source_id: i64,
    config: & Config,
    db: &Database,
    progress: &CountProgress,
    processed: &mut u64)
    -> Result<() > {
    let mut enc_err = Vec::new();

    debug_assert!(path.is_absolute(), "Expected Absolute paths only.");

    // Preflight: already inventoried — attach this source without restatting.
    if let Some(file_id) = db.file_id_by_abs_path(path)? {
        db.add_ref(source_id, file_id)?;
        return Ok(());
    }

    let meta = fs::symlink_metadata(path)
        .map_err(|e| crate::error::Error::io(path, e))?;
    let mode = file_mode(&meta);

    // Extract times, retaining the errors.
    let times = get_file_times(&meta);
    let mtime = strip_transpose(path, times.0, &mut enc_err);
    let atime = strip_transpose(path, times.1, &mut enc_err);
    let ctime = strip_transpose(path, times.2, &mut enc_err);
    let uid = strip_transpose(path, file_uid(&meta), &mut enc_err);
    let gid = strip_transpose(path, file_gid(&meta), &mut enc_err);
    let ftype = strip_transpose(
        path, determine_file_type(&meta, &path), &mut enc_err);
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

    if db.insert_file_and_ref(source_id, &NewFileRecord {
        abs_path: path.clean().to_path_buf(),
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
    })? {
        *processed += 1;
        progress.inc(1);
        // TODO deal with the error vec!
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
fn file_uid(md: &fs::Metadata) -> io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    Ok(md.uid())
}

#[cfg(not(unix))]
fn file_uid(_md: &fs::Metadata) -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file uid is not available on this platform",
    ))
}

#[cfg(unix)]
fn file_gid(md: &fs::Metadata) -> io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    Ok(md.gid())
}

#[cfg(not(unix))]
fn file_gid(_md: &fs::Metadata) -> io::Result<u32> {
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
fn determine_file_type(md: &fs::Metadata, path: &Path) -> io::Result<FileType> {
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
        Ok(FileType::Symlink(resolve_link(&path)?))
    } else {
        Ok(FileType::Unknown)
    }
}

#[cfg(windows)]
fn determine_file_type(md: &fs::Metadata, path: &Path) -> io::Result<FileType> {
    use std::os::windows::fs::FileTypeExt;
    let ft = md.file_type();

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