use std::fs::DirEntry;
use std::os::unix::fs::FileTypeExt;

use chrono::{DateTime, Utc};
use walkdir::WalkDir;

use crate::config::Config;
use crate::db::types::{FileType, LinkType, NewFileRecord};
use crate::db::Database;
use crate::error::Result;
use crate::progress::CountProgress;
use crate::shutdown::Shutdown;
use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use path_clean::PathClean;

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    tracing::info!(root = %config.input_dir.display(), "inventory pass");
    let progress = CountProgress::new("inventory");
    let mut inserted = 0u64;

    for entry in WalkDir::new(&config.input_dir)
            .follow_links(false) // Feature: follow symlinks
            .into_iter()
            .filter_map(|e| e.ok()) {

        shutdown.check_between_files()?;
        // TODO deal with special files.
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let rel = path
            .strip_prefix(&config.input_dir)
            .unwrap_or(path)
            .to_path_buf();
        let meta = std::fs::metadata(path).map_err(|e| crate::error::Error::io(path, e))?;

        if db.insert_file(&NewFileRecord {
            rel_path: rel,
            size: meta.len(),
            mtime: file_mtime(&meta),
            atime: file_atime(&meta),
            uid: file_uid(path),
            gid: file_gid(path),
            mode: Some(file_mode(&meta)),
        })? {
            inserted += 1;
            progress.inc(1);
        }
    }

    progress.finish("inventory complete");
    tracing::info!(inserted, total = db.count_files()?, "inventory indexed");
    Ok(())
}


// TODO: Propagate errors.
fn file_mtime(meta: &std::fs::Metadata) -> Option<DateTime<Utc>> {
    meta.modified().ok().map(DateTime::<Utc>::from)
}

fn file_atime(meta: &std::fs::Metadata) -> Option<DateTime<Utc>> {
    meta.accessed().ok().map(DateTime::<Utc>::from)
}

// Just testing if I can write proper rust code myself
fn file_ctime(meta: &std::fs::Metadata) -> Option<DateTime<Utc>> {
    meta.created().ok().map(DateTime::<Utc>::from)
}

#[cfg(unix)]
fn file_uid(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.uid())
}

#[cfg(not(unix))]
fn file_uid(_path: &Path) -> Option<u32> {
    None
}

#[cfg(unix)]
fn file_gid(path: &Path) -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|m| m.gid())
}

#[cfg(not(unix))]
fn file_gid(_path: &Path) -> Option<u32> {
    None
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
/// If a link is a part of a link cycle, a Cycle is emitted
/// If a link returns a NotFound Error, Dangling is returned
/// If a link target cannot be resolved (any other error e.g. permission error), a Unknown is return
#[cfg(unix)]
fn resolve_link(e: DirEntry) -> LinkType {
    let mut visited = HashSet::new();
    let mut current = e.path();
    assert!(current.is_symlink(), "INVARIANT: Non-Link DirEntry supplied");
    assert!(current.is_absolute(), "INVARIANT: Non-Absolute Path supplied");


    loop {
        // Cycle prevention.
        if !visited.insert(current.clone()) {
            return LinkType::Cycle; // cycle detected
        }

        let ft = match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                // Deal with next step resolution.
                let target = std::fs::read_link(&current);
                match target {
                    Ok(pb) => {
                        current = resolve_relative(&current, &pb);
                        continue;
                    }
                    Err(e) => {
                        let fmt_path = current.as_path().as_os_str().to_string_lossy();
                        tracing::warn!("Resolving {fmt_path} resulted an error: {e}");
                        return LinkType::Unknown;
                    }
                }
            }
            Ok(meta) => meta.file_type(),
            Err(e) if e.kind() == ErrorKind::NotFound => return LinkType::Dangling,
            Err(e) => return LinkType::Unknown,
        };

        // Match valid target
        if ft.is_file() {
            return LinkType::File;
        } else if ft.is_dir() {
            return LinkType::Directory;
        } else if ft.is_fifo() {
            return LinkType::FIFO;
        } else if ft.is_char_device() {
            return LinkType::CharacterDevice;
        } else if ft.is_block_device() {
            return LinkType::BlockDevice;
        } else if ft.is_socket() {
            return LinkType::Socket;
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
fn determine_file_type(e: DirEntry) -> FileType {
    let ft = match e.file_type() {
        Ok(o) => o,
        Err(e) => {
            println!("Failed to resolve file type {}", e);
            return FileType::Unknown
        },
    };
    // Iterate through all possible file types
    if ft.is_file() {
        FileType::File
    } else if ft.is_dir() {
        FileType::Directory
    } else if ft.is_fifo() {
        FileType::FIFO
    } else if ft.is_block_device() {
        FileType::BlockDevice
    } else if ft.is_char_device() {
        FileType::CharacterDevice
    } else if ft.is_symlink() {
        return FileType::Symlink(resolve_link(e));
    } else {
        FileType::Unknown
    }
}

#[cfg(windows)]
fn determine_file_type(e: DirEntry) -> FileType {
    let ft = match e.file_type() {
        Ok(o) => o,
        Err(e) => println!("Failed to resolve file type {}", e),
    };
    // Iterate through all possible file types
    if ft.is_file() {
        FileType::File
    } else if ft.is_dir() {
        FileType::Directory
    } else if ft.is_symlink_dir() {
        FileType::Symlink(LinkType::Directory)
    } else if ft.is_symlink_file() {
        FileType::Symlink(LinkType::File)
    } else {
        FileType::Unknown
    }
}