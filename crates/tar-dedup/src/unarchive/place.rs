//! Place: copy/link cached payloads to final output paths.

use crate::config::ExtractConfig;
use crate::db::Database;
use crate::db::types::{FileId, FilePhase, FileRecord};
use crate::error::{Error, Result};
use crate::progress::ByteProgress;
use crate::shutdown::Shutdown;
use nix::NixPath;
use path_clean::PathClean;
use rayon::prelude::*;
use rayon::{ThreadPool, ThreadPoolBuilder};
use std::cmp::min;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

const BATCH_SIZE: u64 = 10_000;

// TODO Logging
// TODO Progress

pub fn run2(config: &ExtractConfig, db: &Database, shutdown: &Shutdown) -> Result<()> {
    let files: Vec<FileRecord> = db.list_files_to_restore()?;
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let progress = ByteProgress::new("extract", total_bytes);

    eprintln!(
        "extract: materializing {} file(s) under {}",
        files.len(),
        config.paths.extraction_root().display()
    );

    for record in files {
        shutdown.check_between_files()?;

        let tar_name = record
            .tar_member_name()
            .expect("Invariant Error: FileRecord without sha1 found!");
        let cache_path = config.paths.extract_cache_dir().join(&tar_name);
        if !cache_path.is_file() {
            return Err(Error::Config(format!(
                "missing cached tar member `{tar_name}` for {}",
                record.abs_path.display()
            )));
        }

        //let dest = safe_output_path(config.paths.extraction_root(), &record.abs_path)?;
        // if let Some(parent) = dest.parent() {
        //     fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        // }

        progress.set_file("extract", &record.abs_path);
        //fs::copy(&cache_path, &dest).map_err(|e| Error::io(&dest, e))?;
        // Lightweight mtime/owner until the permissions stage owns full metadata restore.
        //apply_basic_metadata(config, &record, &dest)?;
        db.mark_file_phase(record.id, FilePhase::AtDestination)?;
        progress.inc(record.size);
    }

    progress.finish("extract place complete");
    Ok(())
}

pub fn run(config: &ExtractConfig, db: &Database, shutdown: &Shutdown) -> Result<()> {
    // Step 1.1 empty out source root prior to starting extraction.
    if config.placement.clean_target {
        assert!(!config.placement.no_create_dir,
                "INVARIANT ERROR: clean_target => no_create_dir is false");
        fs::remove_dir_all(config.paths.extraction_root())?;
        fs::create_dir_all(config.paths.extraction_root())?;
    }

    // Step 1.2 Create directoris if required
    if !config.placement.no_create_dir {
        prepare_extraction_dir(&db, &config, shutdown)?;
    }

    // Step 2, move the canonical files into place for link_tree
    if config.placement.link_tree {
        tracing::info!("Moving canonical file in place for link tree...");
        // TODO move that shit
    }

    Ok(())
}

/// Function walks all the extraction directories, ensures there are no symlink on the path if
/// selected, and errors out or r&r any given path entry that was not dir. If path segment does not
/// exist, path es created.
/// PRECONDITION: no_create_dir is false.
pub fn prepare_extraction_dir(db: &Database, config: &ExtractConfig, shutdown: &Shutdown)
    -> Result<()> {
    debug_assert!(config.paths.extraction_root().is_absolute(),
                  "INVARIANT ERROR: extraction root is not absolute");
    // Prepare helper table
    prepare_dir_look_up_table(&db, &config, &shutdown)?;

    // Create dir in relative mode.
    if !config.placement.absolute_names {
        let mut last_source_id = 0i64;
        loop {
            let sources = db.list_sources(
                Some(true), Some(last_source_id), BATCH_SIZE)?;
            if sources.is_empty() { break }
            last_source_id = sources.last().unwrap().id;

            for source in sources {
                let (checked_prefix, _stripped) = strip_leading_up(
                    &source.original_path.clean());
                let source_base_dir = config.paths.extraction_root().join(checked_prefix);
                let mut checked_path = source_base_dir.clone();

                let mut last_dir = FileId(0);
                loop {
                    let dirs = db.list_directories_from_prep(
                        Some(last_dir), BATCH_SIZE, Some(source.id))?;
                    if dirs.is_empty() { break };
                    last_dir = dirs.last().unwrap().id;

                    for dir in dirs {
                        shutdown.check_in_flight()?;
                        let rel_stem = dir.abs_path.strip_prefix(&source.abs_path)
                            .expect("Same Source => Same abs_path prefix");
                        let target = source_base_dir.join(&rel_stem);
                        build_path(&config, &mut checked_path, &target)?;
                    }
                }
            }
        }
    // Create directories in absolute mode.
    } else {
        let mut last_dir = FileId(0);
        let mut previous_check = PathBuf::new();
        loop {
            let dirs = db.list_directories_from_prep(
                Some(last_dir), BATCH_SIZE, None)?;
            if dirs.is_empty() { break }
            last_dir = dirs.last().unwrap().id;

            for dir in dirs {
                shutdown.check_in_flight()?;
                debug_assert!(dir.abs_path.is_absolute(),
                              "INVARIANT ERROR: target_dir path is not absolute");

                let target_dir = config.paths.extraction_root().join(
                    dir.abs_path.strip_prefix("/").unwrap());
                build_path(config, &mut previous_check, &target_dir)?;
            }
        }
    }
    db.drop_prep_ancestor_table()?;
    Ok(())
}

/// Strip leading ../ in relative paths s.t. they do not escape from the extraction target.
fn strip_leading_up(path: &Path) -> (PathBuf, u64) {
    let mut components = path.components().peekable();
    let mut ups = 0u64;
    while matches!(components.peek(), Some(Component::ParentDir)) {
        components.next();
        ups += 1;
    }
    let mut out = PathBuf::new();
    for comp in components {
        if let Component::Normal(name) = comp {
            out.push(name);
        }
    }
    (out, ups)
}

/// Build a given directory path for later extraction.
/// PRECONDITION: Calling function must ensure no_create_dir is false
pub fn build_path(config: &ExtractConfig, already_checked: &mut PathBuf, target: &Path) -> Result<()>  {
    debug_assert!(!config.placement.no_create_dir,
                  "INVARIANT ERROR: build_path may not be called with no_create_dir");
    let mut prefix = PathBuf::new();
    let mut start = 0u64;
    let iter = already_checked
        .components()
        .zip(target.components())
        .enumerate();

    // Check known good prefix
    for (num, (c_comp, t_comp)) in iter {
        if c_comp == t_comp {
           prefix.push(t_comp)
        } else {
            start = num as u64;
            break
        }
    }
    assert!(prefix.len() >= config.paths.extraction_root().len(), "target not within extract dir");

    // Walk new prefix
    for (num, component) in target.components().enumerate() {
        if (num as u64) < start { continue }
        prefix.push(component);
        let printable = prefix.display();

        // Symlink
        if prefix.exists() && prefix.is_symlink() && !config.placement.keep_dir_symlink {
            fs::remove_file(&prefix)?;
            fs::create_dir_all(&prefix)?;
            tracing::info!("Replaced symlink with dir at path: {printable}");
            continue
        }

        // Some but no dir
        if prefix.exists() && !prefix.is_dir() {
            if config.placement.remove_and_replace {
                fs::remove_file(&prefix)?;
                fs::create_dir_all(&prefix)?;
                tracing::info!("Replaced non-dir with dir at path: {printable}");
                continue
            } else {
                return Err(Error::Config(
                    format!("Encountered existing non-directory path at extraction \
                             location where directory was needed: {printable}")));
            }
        }

        // Does not exist
        if !prefix.exists() {
            // INFO: No further checks: no_create_dir is false
            fs::create_dir_all(&prefix)?;
            tracing::info!("Replaced non-dir with dir at path: {printable}");
            continue;
        }

        // By exclusion principle -> dir or symlink with keep_dir_symlink
        assert!(prefix.exists()
                && (prefix.is_dir()
                || (prefix.is_symlink() && config.placement.keep_dir_symlink)),
                "INVARIANT ERROR: Directory should be created or error raised.")
    }

    *already_checked = target.to_path_buf();
    Ok(())
}

/// Placement contract:
/// We denote the root extraction dir eith `<ed>` e.g. `/home/user/Desktop/archive`
///
/// If absolute paths are given or absolute_names is true, the following happens
/// `<ed>/var/docker/cache/...`
/// Effectively anything that was under `/` on the scanned system now lands in `<ed>/`
///
/// Relative downwards paths are also mapped directly with a relative prevfix.
/// e.g. `--input-dir ./a/b/c` will be mapped to `<ed>/a/b/c`
/// current dir will also map to root so `--input-dir ./` is `<ed>/`
/// paths that only move up will also convert to extraction root, so
/// `../` or `../../` (or any number of parent dir) will all map to `<ed>/`
///
/// Relative paths up and down again map to the downwards prefix.
/// `../../other/directory` turns into `<ed>/other/directory`
///
/// If multiple files map to the same directory, the tool will not complain and simply the first
/// entry to extract to it, will own the path.
pub fn populate_out_tree(db: &Database, config: &ExtractConfig, shutdown: &Shutdown) -> Result<()> {
    debug_assert!(config.paths.extraction_root().is_absolute(),
                  "INVARIANT ERROR: extraction root is not absolute");
    debug_assert!(!db.out_tree_is_built()?, "PRECONDITION FAILED: out tree built");

    if config.placement.absolute_names {
        populate_out_tree_abs(db, config, shutdown)?;
    } else {
        populate_out_tree_rel(db, config, shutdown)?;
    }

    ensure_parent(db)?;
    db.set_out_tree_built()?;
    Ok(())
}

/// Ensures parent exists for all non-directory rows inside the out_tree. This is needed in case
/// a `--files-fromm` file had a file with `/path/to/not/covered/directory/file.txt`
/// where `/path/to/other/*` is covered by recursive index. Creating file.txt would fail because
/// there's no parent path.
pub fn ensure_parent(db: &Database) -> Result<()> {
    let mut current_parents: HashSet<PathBuf> = HashSet::new();
    let mut parent_rows: Vec<NewOutTreeRow> = Vec::new();
    let mut last_id = OutTreeId(0);

    loop {
        let entries = db.list_out_tree(
            last_id, BATCH_SIZE, None, Some(false))?;
        if entries.is_empty() { break }
        last_id = entries.last().expect("PRECONDITION FAILED: Must have at least one").id;

        for entry in entries {
            let par = entry.abs_path.parent().expect("File needs Parent");
            current_parents.insert(par.to_path_buf());
        }

        for parent in &current_parents {
            parent_rows.push(NewOutTreeRow {
                abs_path: parent.clone(),
                file_id: None,
                flags: OutTreeFlags::default(),
            })
        }
        db.insert_out_tree_rows(&parent_rows)?;
        // Clear the accumulators before the next run to avoid huge structures in ram.
        current_parents.clear();
        parent_rows.clear();
    }

    Ok(())
}

/// Build the out_tree table, if the user selected absolute names for the materialization method.
fn populate_out_tree_abs(db: &Database, config: &ExtractConfig, shutdown: &Shutdown) -> Result<()> {
    debug_assert!(config.placement.absolute_names,
        "PRECONDITION FAILED: Function builds absolute names");
    let root = config.paths.extraction_root();
    let mut last_id = FileId(0);

    loop {
        shutdown.check_in_flight()?;
        let entries = db.list_materialized_entries(
            Some(last_id), BATCH_SIZE, None, Some(false))?;
        if entries.is_empty() {
            break
        }
        last_id = entries.last().expect("non-empty batch").id;

        // Process the entries
        let processed: Vec<NewOutTreeRow> = build_new_out_tree_rows(
            &entries, &root, None);

        db.insert_out_tree_rows(&processed)?;
        // INFO ref table is left empty since we are working with abs_paths
    }
    Ok(())
}

/// Build the out_tree table, if the user selected no absolute names for the materialization method.
/// Paths are populated on first-come-first-serve basis. I.e. if the user used
/// --no-strict-separation, it is possible that parts of the tree were mapped twice and those names
/// might collide. The sources are selected in ascending order (same order as adding and scanning
/// initially) and their subtree is then materialized at its relative target.
fn populate_out_tree_rel(db: &Database, config: &ExtractConfig, shutdown: &Shutdown) -> Result<()> {
    let root = config.paths.extraction_root();
    let mut last_source_id = 0i64;

    loop {
        let sources = db.list_sources(None, last_source_id, BATCH_SIZE)?;
        if sources.is_empty() {
            break;
        }
        last_source_id = sources.last().expect("non-empty batch").id;

        for source in sources {
            let san_org_path = source.original_path.clean();

            let extraction_base: PathBuf = if san_org_path.is_absolute() {
                // Got root dir, simply return the extract root
                if san_org_path == PathBuf::from("/") {
                    root.to_path_buf()
                } else {
                    let stripped = san_org_path
                        .strip_prefix("/")
                        .expect("Absolute expects / at the beginning");
                    root.join(stripped)
                }
            } else {
                let (cut, _) = strip_leading_up(&san_org_path);
                debug_assert!(!cut.starts_with("/"), "Relative does not expect a / at begin");
                root.join(cut)
            };

            // File Loop
            let mut last_id = FileId(0);
            loop {
                shutdown.check_in_flight()?;
                let entries = db.list_materialized_entries(
                    Some(last_id), BATCH_SIZE, Some(source.id), Some(false)
                )?;
                if entries.is_empty() { break }
                last_id = entries.last().expect("non-empty batch").id;

                let processed: Vec<NewOutTreeRow> = build_new_out_tree_rows(
                    &entries, &root, Some((&source.abs_path, &extraction_base)));

                let out_ids = db.insert_out_tree_rows(&processed)?;
                let ref_pairs: Vec<(OutTreeId, i64)> = out_ids
                    .iter()
                    .map(|id| { (id.clone(), source.id) })
                    .collect();
                db.insert_ref_out_rows(&ref_pairs)?;
            }
        }
    }
    Ok(())
}

/// Given a vectors of StrippedRecords compute the new OutTreeRows
fn build_new_out_tree_rows(
    entries: &Vec<StrippedRecord>, root: &Path, sources: Option<(&Path, &Path)>)
                           -> Vec<NewOutTreeRow> {
    let processed: Vec<NewOutTreeRow> = entries
        .iter()
        .map(|r| {
            let path = catalog_to_target_abs(root, &r.abs_path, sources);
            let is_dir = match r.ftype {
                None => false,
                Some(t) => t == FileType::Directory
            };
            let mut of = OutTreeFlags::default();
            of.set(OutTreeFlag::IsDirectory, is_dir);
            NewOutTreeRow {
                abs_path: path.clone(),
                file_id: Some(r.id),
                flags: of,
            }
        })
        .collect();
    processed
}

/// Build the abs path where a given file is extracted to.
fn catalog_to_target_abs(
    extraction_root: &Path,
    catalog_path: &Path,
    relative_component: Option<(&Path, &Path)>,
) -> PathBuf {
    match relative_component {
        None => {
            let rel = catalog_path
                .strip_prefix("/")
                .expect(&format!(
                    "INVARIANT ERROR: Catalogue Path MUST be absolute and start with /, got {}",
                    catalog_path.display()
                ));
            extraction_root.join(rel)
        }
        Some((source_abs, source_base)) => {
            let rel_stem = catalog_path
                .strip_prefix(source_abs)
                .expect("Source rows must start within source root");
            source_base.join(rel_stem)
        }
    }
}