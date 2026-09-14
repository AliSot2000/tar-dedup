//! Placement prologue: all database-only preparation before any filesystem work.
//!
//! Runs under its own `PlacementPrologue` phase and is guarded by the
//! `placement_prologue_done` meta flag. Populates `files.new_name` (name
//! stripping) and builds the `out_tree` (+ hardlink-canonical election). It
//! never touches the filesystem — directory creation (`build_path`),
//! materialization and cleanup stay in `place`.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use path_clean::PathClean;

use crate::cli::HardLinkGrouping;
use crate::common::transform::{TransformExpr, TransformSource, parse_transform_expr};
use crate::config::ExtractConfig;
use crate::db::flags::{OutTreeFlag, OutTreeFlags};
use crate::db::types::{FileId, FileType, NewOutTreeRow, OutTreeId, StrippedRecord};
use crate::db::Database;
use crate::error::Result;
use crate::shutdown::Shutdown;

const BATCH_SIZE: u64 = 10_000;

/// Run the pure-DB placement preparation once (idempotent via meta flag).
pub fn run(config: &ExtractConfig, db: &Database, shutdown: &Shutdown) -> Result<()> {
    if db.placement_prologue_done()? {
        return Ok(());
    }
    let transform = resolve_transform(config, db)?;
    // Use the renamed member when a transform or strip is active; otherwise build
    // the out_tree from `abs_path` as before.
    let use_new_name = transform.is_some() || config.strip_components > 0;
    if use_new_name {
        populate_new_names(db, config, shutdown, transform.as_ref())?;
    }
    if !db.out_tree_is_built()? {
        populate_out_tree(db, config, shutdown, use_new_name)?;
    }
    // Canonical election is DB-only however meaningless in `--link-tree` mode
    // (same branch as the pre-refactor `place::run`).
    if !config.placement.link_tree {
        prepare_hardlink_canonicals(config, db)?;
    }
    db.set_placement_prologue_done()?;
    Ok(())
}

/// Populate `files.new_name` with the member-relative stripped name. Recomputed
/// Resolve the effective transform from the CLI policy / archived meta.
fn resolve_transform(config: &ExtractConfig, db: &Database) -> Result<Option<TransformExpr>> {
    match &config.transform_policy {
        TransformSource::None => Ok(None),
        TransformSource::Cli(expr) => parse_transform_expr(expr).map(Some),
        TransformSource::Stored => match db.get_archive_transform()? {
            Some(expr) => parse_transform_expr(&expr).map(Some),
            None => Ok(None),
        },
    }
}
/// Populate `files.new_name` with the member-relative record name. Recomputed
/// from `abs_path` on every prologue run (never chains onto an already-mapped
/// `new_name`), so re-running with the same or changed `--strip-components` is
/// deterministic. The strip happens strictly on the relative member, i.e. AFTER
/// the absolute path is converted to relative.
fn populate_new_names(db: &Database, config: &ExtractConfig, shutdown: &Shutdown) -> Result<()> {
    let strip = config.strip_components;
    if config.placement.absolute_names {
        let mut last_id = FileId(0);
        loop {
            shutdown.check_in_flight()?;
            let entries: Vec<StrippedRecord> = db.list_materialized_entries(
                Some(last_id), BATCH_SIZE, None, None)?;
            if entries.is_empty() { break }
            last_id = entries.last().expect("non-empty batch").id;
            apply_renames(db, &entries, strip, |abs: &Path| {
                abs.strip_prefix("/").expect("abs path starts with /").to_path_buf()
            })?;
        }
        return Ok(());
    }

    let mut last_source_id = 0i64;
    loop {
        let sources = db.list_sources(None, last_source_id, BATCH_SIZE)?;
        if sources.is_empty() { break }
        last_source_id = sources.last().expect("non-empty batch").id;

        for source in sources {
            let source_abs = source.abs_path;
            let mut last_id = FileId(0);
            loop {
                shutdown.check_in_flight()?;
                let entries: Vec<StrippedRecord> = db.list_materialized_entries(
                    Some(last_id), BATCH_SIZE, Some(source.id), None)?;
                if entries.is_empty() { break }
                last_id = entries.last().expect("non-empty batch").id;
                apply_renames(db, &entries, strip, |abs: &Path| {
                    abs.strip_prefix(&source_abs)
                        .expect("entry must start within source root")
                        .to_path_buf()
                })?;
            }
        }
    }
    Ok(())
}

fn apply_renames(
    db: &Database,
    entries: &[StrippedRecord],
    strip: u32,
    member_of: impl Fn(&Path) -> PathBuf,
) -> Result<()> {
    for entry in entries {
        let member = member_of(&entry.abs_path);
        // None: no rename applies.
        // Some(""): member collapses (<= strip components) -> skip sentinel.
        // Some(rel): the member-relative stripped name.
        let new_name = strip_relative_member(&member, strip);
        db.set_file_new_name(entry.id, new_name.as_deref())?;
    }
    Ok(())
}

/// Strip up to `strip` leading components of a relative member path.
/// `0` is a no-op (`None`). A member with `<= strip` components collapses to
/// `Some("")`, matching GNU tar's "transforms to empty name" skip.
fn strip_relative_member(member: &Path, strip: u32) -> Option<String> {
    if strip == 0 {
        return None;
    }
    let comps: Vec<&OsStr> = member
        .components()
        .filter_map(|comp| match comp {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    if comps.len() as u32 <= strip {
        return Some(String::new());
    }
    let kept = &comps[strip as usize..];
    if kept.is_empty() {
        return Some(String::new());
    }
    Some(
        kept.iter()
            .map(|name| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/"),
    )
}

// -------------------------------------------------------------------------------------------------
// Out-tree build (moved from `place`; still pure DB)
// -------------------------------------------------------------------------------------------------

/// Placement contract:
/// We denote the root extraction dir with `<ed>` e.g. `/home/user/Desktop/archive`
///
/// If absolute paths are given or absolute_names is true, the following happens
/// `<ed>/var/docker/cache/...`
/// Effectively anything that was under `/` on the scanned system now lands in `<ed>/`
///
/// Relative downwards paths are also mapped directly with a relative prefix.
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

/// Build the out_tree table, if the user selected absolute names for the materialization method.
fn populate_out_tree_abs(db: &Database, config: &ExtractConfig, shutdown: &Shutdown) -> Result<()> {
    debug_assert!(config.placement.absolute_names,
                  "PRECONDITION FAILED: Function builds absolute names");
    let root = config.paths.extraction_root();
    let mut last_id = FileId(0);

    loop {
        shutdown.check_in_flight()?;
        let entries: Vec<StrippedRecord> = db.list_materialized_entries(
            Some(last_id), BATCH_SIZE, None, Some(false))?;
        if entries.is_empty() { break }
        last_id = entries.last().expect("non-empty batch").id;

        // Process the entries
        let processed: Vec<NewOutTreeRow> = build_new_out_tree_rows(
            &entries, &root, None, config.strip_components > 0);

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
                let entries: Vec<StrippedRecord> = db.list_materialized_entries(
                    Some(last_id), BATCH_SIZE, Some(source.id), Some(false)
                )?;
                if entries.is_empty() { break }
                last_id = entries.last().expect("non-empty batch").id;

                let processed: Vec<NewOutTreeRow> = build_new_out_tree_rows(
                    &entries, &root, Some((&source.abs_path, &extraction_base)),
                    config.strip_components > 0);

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
    entries: &Vec<StrippedRecord>, root: &Path, sources: Option<(&Path, &Path)>,
    use_new_name: bool,
) -> Vec<NewOutTreeRow> {
    let base = match sources {
        Some((_, base)) => base.to_path_buf(),
        None => root.to_path_buf(),
    };
    let mut processed: Vec<NewOutTreeRow> = Vec::new();
    for r in entries {
        let path = if use_new_name {
            match &r.new_name {
                Some(nn) if !nn.is_empty() => base.join(nn),
                // Empty name => skip (GNU transforms-to-empty semantics).
                Some(_) => continue,
                None => catalog_to_target_abs(root, &r.abs_path, sources),
            }
        } else {
            catalog_to_target_abs(root, &r.abs_path, sources)
        };
        let is_dir = r.ftype == FileType::Directory;
        let mut of = OutTreeFlags::default();
        of.set(OutTreeFlag::IsDirectory, is_dir);
        processed.push(NewOutTreeRow {
            abs_path: path,
            file_id: Some(r.id),
            flags: of,
        });
    }
    processed
}

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

/// Ensures parent exists for all non-directory rows inside the out_tree. This is needed in case
/// a `--files-fromm` file had a file with `/path/to/not/covered/directory/file.txt`
/// where `/path/to/other/*` is covered by recursive index. Creating file.txt would fail because
/// there's no parent path.
fn ensure_parent(db: &Database) -> Result<()> {
    let mut current_parents: HashSet<PathBuf> = HashSet::new();
    let mut parent_rows: Vec<NewOutTreeRow> = Vec::new();
    let mut last_id = OutTreeId(0);
    let mut flags = OutTreeFlags::default();
    flags.set(OutTreeFlag::IsDirectory, true);
    let cref_flags = &flags;

    loop {
        let entries = db.list_out_tree(
            last_id, BATCH_SIZE, None, Some(false))?;
        if entries.is_empty() { break }
        last_id = entries.last().expect("PRECONDITION FAILED: Must have at least one").id;

        for entry in entries {
            let par = entry.abs_path.parent().expect("File NEEDs Parent");
            current_parents.insert(par.to_path_buf());
        }

        for parent in &current_parents {
            parent_rows.push(NewOutTreeRow {
                abs_path: parent.clone(),
                file_id: None,
                flags: cref_flags.clone(),
            })
        }
        db.insert_out_tree_rows(&parent_rows)?;
        // Clear the accumulators before the next run to avoid huge structures in ram.
        current_parents.clear();
        parent_rows.clear();
    }

    Ok(())
}

/// Elect hard-link canonicals in the out_tree (mark_canonical family of updates).
fn prepare_hardlink_canonicals(config: &ExtractConfig, db: &Database) -> Result<()> {
    let updates: u64 = match config.placement.hard_link_grouping {
        HardLinkGrouping::None => db.mark_all_canonical()?,
        HardLinkGrouping::Global => db.mark_global_canonical()?,
        HardLinkGrouping::Source => {
            let mut last_id = 0i64;
            let mut sum = 0u64;
            loop {
                let sources = db.list_sources(None, last_id, BATCH_SIZE)?;
                if sources.is_empty() { break }
                last_id = sources
                    .last()
                    .expect("PRECONDITION FAILED: At least one element expected").id;

                for source in sources {
                    sum += db.mark_source_canonical(source.id)?;
                }
            }
            sum
        }
    };
    tracing::info!("Number of materialized target {updates}");
    Ok(())
}

/// Strip leading ../ in relative paths s.t. they do not escape from the extraction target.
fn strip_leading_up(path: &Path) -> (PathBuf, u64) {
    debug_assert_eq!(path, path.clean(),
                     "INVARIANT ERROR: Function should only work on clean paths");
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

// -------------------------------------------------------------------------------------------------
// Testing
// -------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_relative_member_counts_components() {
        assert_eq!(strip_relative_member(Path::new("a/b/c.txt"), 1),
                   Some("b/c.txt".to_string()));
        assert_eq!(strip_relative_member(Path::new("a/b/c.txt"), 2),
                   Some("c.txt".to_string()));
        assert_eq!(strip_relative_member(Path::new("a/b/c.txt"), 3),
                   Some("".to_string()));
        assert_eq!(strip_relative_member(Path::new("a/b/c.txt"), 9),
                   Some("".to_string()));
    }

    #[test]
    fn strip_relative_member_zero_is_noop() {
        assert_eq!(strip_relative_member(Path::new("a/b"), 0), None);
    }

    #[test]
    fn strip_relative_member_ignores_root_dir() {
        assert_eq!(strip_relative_member(Path::new("/a/b"), 1),
                   Some("b".to_string()));
    }
}