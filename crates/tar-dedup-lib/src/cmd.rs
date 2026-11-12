//! Catalog inspection commands (`inspect`, `list`, `dump`, `query`) and the
//! work-DB `reset` command (scaffolding).
//!
//! The read commands target a finished archive's sqlite footer (`-f`) or an
//! existing work database directly (`--db` / `--work-dir`); `reset` targets a
//! work database whose stored parameters are to be re-pointed. Nothing is
//! implemented beyond the input plumbing yet: every command currently reports
//! itself as `Error::Config(… not implemented)`.

pub mod check;
pub mod dump;
pub mod inspect;
pub mod list;
pub mod query;
pub mod reset;

use std::path::{Path, PathBuf};

use crate::cli::{ArchiveInputArgs, ResetArgs};
use crate::common::defaults::{DB_NAME, SNAPSHOT_INIT_TAR_NAME};
use crate::config::resolve_path_to_abs_path;
use crate::error::{Error, Result};

/// Catalog source resolved from CLI args.
pub enum CatalogSource {
    /// Finished archive; the catalog lives in its seekable sqlite footer.
    Archive(PathBuf),
    /// Existing work database read directly.
    Database(PathBuf),
}

/// Resolve the read commands' input against the current directory and validate
/// that the named archive or database exists. Priority: `--db` > `--work-dir` >
/// `-f`; at least one must be given.
pub fn resolve_catalog_source(args: &ArchiveInputArgs) -> Result<CatalogSource> {
    let cwd = std::env::current_dir().map_err(Error::from)?;
    if let Some(db) = args.db.as_ref() {
        let db = resolve_path_to_abs_path(db, &cwd);
        if !db.is_file() {
            return Err(Error::Config(format!("no database at {}", db.display())));
        }
        return Ok(CatalogSource::Database(db));
    }
    if let Some(dir) = args.work_dir.as_ref() {
        let dir = resolve_path_to_abs_path(dir, &cwd);
        return Ok(CatalogSource::Database(resolve_work_db_path(&dir)?));
    }
    match args.archive.as_ref() {
        Some(archive) => {
            let archive = resolve_path_to_abs_path(archive, &cwd);
            if !archive.is_file() {
                return Err(Error::Config(format!(
                    "archive does not exist or is not a file: {}",
                    archive.display()
                )));
            }
            Ok(CatalogSource::Archive(archive))
        }
        None => Err(Error::Config(
            "one of `-f`, `--db`, or `--work-dir` is required".into(),
        )),
    }
}

/// Resolve a catalog inside a work/product dir. Prefer the disposable work DB;
/// after a finished bypass extract that file is gone, so fall back to the
/// immutable `manifest.sqlite`.
fn resolve_work_db_path(dir: &Path) -> Result<PathBuf> {
    let work_db = dir.join(DB_NAME);
    if work_db.is_file() {
        return Ok(work_db);
    }
    let manifest = dir.join(SNAPSHOT_INIT_TAR_NAME);
    if manifest.is_file() {
        return Ok(manifest);
    }
    Err(Error::Config(format!(
        "no work database or manifest at {}",
        dir.display()
    )))
}

/// Resolve the `reset` target database (`--db` xor `--work-dir`) and validate it.
pub fn resolve_reset_db(args: &ResetArgs) -> Result<PathBuf> {
    let cwd = std::env::current_dir().map_err(Error::from)?;
    let db = match (args.db.as_ref(), args.work_dir.as_ref()) {
        (Some(_), Some(_)) => {
            return Err(Error::Config(
                "--db and --work-dir are mutually exclusive (pass one)".into(),
            ));
        }
        (Some(db), None) => resolve_path_to_abs_path(db, &cwd),
        (None, Some(dir)) => {
            let dir = resolve_path_to_abs_path(dir, &cwd);
            dir.join("tar-dedup.sqlite")
        }
        (None, None) => {
            return Err(Error::Config("reset requires `--work-dir` or `--db`".into()));
        }
    };
    if !db.is_file() {
        return Err(Error::Config(format!("no work database at {}", db.display())));
    }
    Ok(db)
}

/// Shared guard so every command reports itself consistently until implemented.
pub(crate) fn not_implemented(name: &str) -> Error {
    Error::Config(format!("`{name}` is not implemented yet"))
}