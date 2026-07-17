#[cfg(unix)]
use walkdir::DirEntry;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;

use chrono::{DateTime, Utc};
use walkdir::WalkDir;

use crate::config::Config;
use crate::db::types::{FileType, LinkType, NewFileRecord};
use crate::pipeline::xattr::{get_file_xattr, get_file_acl, get_file_selinux_data};
use crate::db::Database;
use crate::error::{FileStatError, Result};
use crate::progress::CountProgress;
use crate::shutdown::Shutdown;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

use path_clean::PathClean;

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    tracing::info!(root = %config.input_dir.display(), "inventory pass");
    let progress = CountProgress::new("inventory");
    let mut inserted = 0u64;
    let mut enc_err = Vec::new();

    for entry in WalkDir::new(&config.input_dir)
            .follow_links(false) // Feature: follow symlinks
            .into_iter()
            .filter_map(|e| e.ok()) {

        shutdown.check_between_files()?;
        enc_err.clear();

        let path = entry.path();
        let rel = path
            .strip_prefix(&config.input_dir)
            .unwrap_or(path)
            .to_path_buf();
        let meta = std::fs::metadata(path)
            .map_err(|e| crate::error::Error::io(path, e))?;

        // Extract times, retaining the errors.
        let mtime = strip_transpose(path, file_mtime(&meta), &mut enc_err);
        let atime = strip_transpose(path, file_atime(&meta), &mut enc_err);
        let ctime = strip_transpose(path, file_ctime(&meta), &mut enc_err);
        let uid = strip_transpose(path, file_uid(& path), &mut enc_err);
        let gid = strip_transpose(path, file_gid(&path), &mut enc_err);
        let ftype = strip_transpose(path, determine_file_type(entry), &mut enc_err);
        let mode = file_mode(&meta);

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
            rel_path: rel,
            size: meta.len(),
            mtime,
            atime,
            ctime,
            uid,
            gid,
            ftype,
            mode,
            xattrs,
            posix_acl,
            selinux_ctx,
        })? {
            inserted += 1;
            progress.inc(1);
        }
    }

    progress.finish("inventory complete");
    tracing::info!(inserted, total = db.count_files()?, "inventory indexed");
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

// TODO: Propagate errors.
fn file_mtime(meta: &std::fs::Metadata) -> io::Result<DateTime<Utc>> {
    Ok(DateTime::<Utc>::from(meta.modified()?))
}

fn file_atime(meta: &std::fs::Metadata) -> io::Result<DateTime<Utc>> {
    Ok(DateTime::<Utc>::from(meta.accessed()?))
}

fn file_ctime(meta: &std::fs::Metadata) -> io::Result<DateTime<Utc>> {
    Ok(DateTime::<Utc>::from(meta.created()?))
}

#[cfg(unix)]
fn file_uid(path: &Path) -> io::Result<u32> {
    use std::os::unix::fs::MetadataExt;
    Ok(std::fs::metadata(path)?.uid())
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
    Ok(std::fs::metadata(path)?.gid())
}

#[cfg(not(unix))]
fn file_gid(_path: &Path) -> io::Result<u32> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file uid is not available on this platform",
    ))
}

#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::MetadataExt;
    meta.mode()
}

#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

/// Function attempts to figure out what a given soft link (chain) is pointing to.
/// If a link is a part of a link cycle, a `Cycle` is emitted
/// If a link returns a NotFound Error, `Dangling` is returned
/// If a link target cannot be resolved (any other error e.g. permission error), `Unknown` is return
#[cfg(unix)]
fn resolve_link(e: DirEntry) -> io::Result<LinkType> {
    let mut visited = HashSet::new();
    let mut current = e.path();
    assert!(current.is_symlink(), "INVARIANT: Non-Link DirEntry supplied");
    assert!(current.is_absolute(), "INVARIANT: Non-Absolute Path supplied");

    loop {
        // Cycle prevention.
        if !visited.insert(current.clone()) {
            return Ok(LinkType::Cycle); // cycle detected
        }

        let ft = match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                // Deal with next step resolution.
                let target = std::fs::read_link(&current);
                match target {
                    Ok(pb) => {
                        current = resolve_relative(current, pb.as_path()).as_path();
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
        if ft.is_file() {
            return Ok(LinkType::File);
        } else if ft.is_dir() {
            return Ok(LinkType::Directory);
        } else if ft.is_fifo() {
            return Ok(LinkType::FIFO);
        } else if ft.is_char_device() {
            return Ok(LinkType::CharacterDevice);
        } else if ft.is_block_device() {
            return Ok(LinkType::BlockDevice);
        } else if ft.is_socket() {
            return Ok(LinkType::Socket);
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
fn determine_file_type(e: DirEntry) -> io::Result<FileType> {
    let ft = match e.file_type() {
        Ok(o) => o,
        Err(e) => {
            println!("Failed to resolve file type {}", e);
            return Ok(FileType::Unknown)
        },
    };
    // Iterate through all possible file types
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
        return Ok(FileType::Symlink(resolve_link(e)?));
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