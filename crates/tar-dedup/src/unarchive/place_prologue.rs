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
use crate::common::batched_stepped_loop;
use crate::common::transform::{TransformExpr, TransformSource, parse_transform_expr};
use crate::db::Database;
use crate::db::flags::{OutTreeFlag, OutTreeFlags};
use crate::db::types::{FileId, FileType, NewOutTreeRow, OutTreeId, SourceRecord, StrippedRecord};
use crate::error::Result;
use crate::unarchive::ExtractRTArgs;

/// Run the pure-DB placement preparation once (idempotent via meta flag).
pub fn run(rt: &ExtractRTArgs) -> Result<()> {
    // Resume display: the out_tree already exists, show its real size.
    rt.progress.set_phase_total(rt.db.count_entries()?);
    if rt.db.placement_prologue_done()? {
        return Ok(());
    }
    let transform = resolve_transform(rt)?;
    // Use the renamed member when a transform or strip is active; otherwise build
    // the out_tree from `abs_path` as before.
    let use_new_name = transform.is_some() || rt.config.strip_components > 0;
    if use_new_name {
        tracing::info!("Apply name transformation");
        populate_new_names(rt, transform.as_ref())?;
    }
    if !rt.db.out_tree_is_built()? {
        tracing::info!("Populating out tree");
        populate_out_tree(rt, use_new_name)?;
    }
    rt.progress.set_phase_total(rt.db.count_out_tree_rows()?);
    // Canonical election is DB-only. It is meaningless `--link-tree` mode
    // (same branch as the pre-refactor `place::run`).
    if !rt.config.placement.link_tree {
        tracing::info!("Determining hardlink canonicals");
        prepare_hardlink_canonicals(rt)?;
    }
    rt.db.set_placement_prologue_done()?;
    let count = rt.db.count_out_tree_rows()?;
    tracing::info!(
        "placement_prologue done: processed {count} of {count}, {} error(s)",
        rt.db.count_errors(Some("placement_prologue"))?,
    );
    Ok(())
}

// -------------------------------------------------------------------------------------------------
// Stages
// -------------------------------------------------------------------------------------------------

/// Resolve the effective transform from the CLI policy / archived meta.
fn resolve_transform(rt: &ExtractRTArgs) -> Result<Option<TransformExpr>> {
    let config = rt.config;
    let db = rt.db;
    match &config.transform_policy {
        TransformSource::None => Ok(None),
        TransformSource::Cli(expr) => parse_transform_expr(expr).map(Some),
        TransformSource::Stored => match db.get_archive_transform()? {
            Some(expr) => parse_transform_expr(&expr).map(Some),
            None => Ok(None),
        },
    }
}

/// Populate the new_name field using GNU ordering (transform then strip). The
/// pre-mangling member is the same relative target the regular out_tree mapping
/// produces, so transforms match identically whether or not new-name mode is active.
fn populate_new_names(rt: &ExtractRTArgs, transform: Option<&TransformExpr>)
    -> Result<()> {

    if rt.config.placement.absolute_names {
        let mut apply = |entries: &Vec<StrippedRecord>| -> Result<()> {
            apply_renames(rt, entries, transform, |abs: &Path| abs_rel_path(abs))
        };
        stepped_dirs_then_rest(rt, None, false, &mut apply)
    } else {
        batched_stepped_loop(
            rt.config.process.batch_size,
            || 0i64,
            |lid, bs| rt.db.list_sources(None, *lid, bs),
            |src: &SourceRecord| src.id,
            |sources| {
                for source in sources {
                    let source_abs = source.abs_path;
                    let mut apply = |entries: &Vec<StrippedRecord>| -> Result<()> {
                        apply_renames(rt, entries, transform, |abs: &Path|
                            rel_rel_path(&source_abs, &source.original_path, abs))
                    };
                    stepped_dirs_then_rest(rt, Some(source.id), false, &mut apply)?;
                }
                Ok(())
            }
        )
    }
}

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
fn populate_out_tree(
    rt: &ExtractRTArgs, use_new_name: bool)
    -> Result<()> {
    debug_assert!(rt.config.paths.extraction_root().is_absolute(),
                  "INVARIANT ERROR: extraction root is not absolute");
    debug_assert!(!rt.db.out_tree_is_built()?,
                  "PRECONDITION FAILED: out tree built");

    if rt.config.placement.absolute_names {
        populate_out_tree_abs(rt, use_new_name)?;
    } else {
        populate_out_tree_rel(rt, use_new_name)?;
    }

    ensure_parent(rt.db, rt.progress, rt.config.process.batch_size)?;
    rt.db.set_out_tree_built()?;
    Ok(())
}

/// Elect hard-link canonicals in the out_tree (mark_canonical family of updates).
fn prepare_hardlink_canonicals(rt: &ExtractRTArgs) -> Result<()> {
    let _updates: u64 = match rt.config.placement.hard_link_grouping {
        HardLinkGrouping::None => rt.db.mark_all_canonical()?,
        HardLinkGrouping::Global => rt.db.mark_global_canonical()?,
        HardLinkGrouping::Source => {
            let mut sum = 0u64;
            batched_stepped_loop(
                rt.config.process.batch_size,
                || 0i64,
                |lid, bs| rt.db.list_sources(None, *lid, bs),
                |src| src.id,
                |sources| {
                    for source in sources {
                        sum += rt.db.mark_source_canonical(source.id)?;
                    }
                Ok(())
                }
            )?;
            sum
        }
    };
    Ok(())
}

/// Two-stage driver over `list_materialized_entries`: directory rows first, then
/// the remaining entries. The directory pass bypasses the include/exclude filters
/// when `--overwrite-dir` is set (excluded directories still get out_tree rows so
/// their archived metadata can be restored); the second pass is always
/// filter-aware.
fn stepped_dirs_then_rest(
    rt: &ExtractRTArgs,
    source_id: Option<i64>,
    only_valid_new_name: bool,
    f: &mut impl FnMut(&Vec<StrippedRecord>) -> Result<()>)
    -> Result<()> {

    stepped_materialized(
        rt, source_id, Some(true), !rt.config.attributes.force_overwrite_dir,
        only_valid_new_name, f)?;

    stepped_materialized(
        rt, source_id, Some(false), true, only_valid_new_name, f)?;

    Ok(())
}

/// Single batched pass over one `list_materialized_entries` bucket (`only_dirs`),
/// reporting each batch to `f`.
fn stepped_materialized(
    rt: &ExtractRTArgs,
    source_id: Option<i64>,
    only_dirs: Option<bool>,
    apply_filters: bool,
    only_valid_new_name: bool,
    f: &mut impl FnMut(&Vec<StrippedRecord>) -> Result<()>)
    -> Result<()> {

    batched_stepped_loop(
        rt.config.process.batch_size,
        || FileId(0),
        |lid, bs| rt.db.list_materialized_entries(
            *lid, bs, source_id, only_dirs, only_valid_new_name, apply_filters
        ),
        |r: &StrippedRecord| r.id,
        |entries| f(&entries),
    )
}

// -------------------------------------------------------------------------------------------------
// Substage
// -------------------------------------------------------------------------------------------------

//  TODO: Transaction?
fn apply_renames(
    rt: &ExtractRTArgs,
    entries: &[StrippedRecord],
    transform: Option<&TransformExpr>,
    path_rel_converter: impl Fn(&Path) -> PathBuf)
    -> Result<()> {

    for entry in entries {
        let member = path_rel_converter(&entry.abs_path);
        let mut name = member.to_string_lossy().into_owned();
        if let Some(t) = transform {
            name = t.apply(&name);
        }
        // Strip after transform (GNU order); `strip == 0` leaves the name as-is.
        let name = strip_relative_member(&name, rt.config.strip_components);
        // In new-name mode every row gets a (possibly identity) member; `""`
        // marks the empty-name skip sentinel consumed by the out_tree filter.
        rt.db.set_file_new_name(entry.id, Some(&name))?;
        rt.progress.inc_phase(1);
    }
    
    Ok(())
}

/// Build the out_tree table, if the user selected absolute names for the materialization method.
fn populate_out_tree_abs(
    rt: &ExtractRTArgs, use_new_name: bool) -> Result<()> {
    debug_assert!(rt.config.placement.absolute_names,
                  "PRECONDITION FAILED: Function builds absolute names");

    let mut apply = |entries: &Vec<StrippedRecord>| -> Result<()> {
        // Process the entries
        let root = rt.config.effective_extraction_root();
        let processed: Vec<NewOutTreeRow> = build_new_out_tree_rows(
            entries, &root, None, use_new_name
        );

        rt.db.insert_out_tree_rows(&processed)?;
        rt.progress.inc_phase(processed.len() as u64);
        // INFO ref table is left empty since we are working with abs_paths and one source row can
        //  have at most one child
        Ok(())
    };
    stepped_dirs_then_rest(rt, None, use_new_name, &mut apply)
}

/// Build the out_tree table, if the user selected no absolute names for the materialization method.
/// Paths are populated on first-come-first-serve basis. I.e. if the user used
/// --no-strict-separation, it is possible that parts of the tree were mapped twice and those names
/// might collide. The sources are selected in ascending order (same order as adding and scanning
/// initially) and their subtree is then materialized at its relative target.
fn populate_out_tree_rel(
    rt: &ExtractRTArgs,
    use_new_name: bool)
    -> Result<()> {
    let root = rt.config.effective_extraction_root();
    batched_stepped_loop(
        rt.config.process.batch_size,
        || 0i64,
        |lid, bs| rt.db.list_sources(None, *lid, bs),
        |src| src.id,
        |sources| {
            for source in sources {
                let mut apply = |entries: &Vec<StrippedRecord>| -> Result<()> {
                    let processed: Vec<NewOutTreeRow> = build_new_out_tree_rows(
                        entries, &root, Some((&source.abs_path, &source.original_path)), use_new_name
                    );

                    let out_ids = rt.db.insert_out_tree_rows(&processed)?;
                    let ref_pairs: Vec<(OutTreeId, i64)> = out_ids
                        .iter()
                        .map(|id| { (id.clone(), source.id) })
                        .collect();
                    rt.db.insert_ref_out_rows(&ref_pairs)?;
                    rt.progress.inc_phase(processed.len() as u64);
                    Ok(())
                };
                stepped_dirs_then_rest(rt, Some(source.id), use_new_name, &mut apply)?;
            }
            Ok(())
        }
    )
}

/// Ensures parent exists for all non-directory rows inside the out_tree. This is needed in case
/// a `--files-fromm` file had a file with `/path/to/not/covered/directory/file.txt`
/// where `/path/to/other/*` is covered by recursive index. Creating file.txt would fail because
/// there's no parent path.
fn ensure_parent(db: &Database, progress: &crate::progress::ProgressBarSet, batch_size: u64)
    -> Result<()> {
    let mut current_parents: HashSet<PathBuf> = HashSet::new();
    let mut parent_rows: Vec<NewOutTreeRow> = Vec::new();
    let mut flags = OutTreeFlags::default();
    flags.set(OutTreeFlag::IsDirectory, true);
    let cref_flags = &flags;

    batched_stepped_loop(
        batch_size,
        || OutTreeId(0),
        |lid, bs| db.list_out_tree(
            *lid, bs, None, Some(false)
        ),
        |o_rec| o_rec.id,
        |entries | {
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
            progress.inc_phase(parent_rows.len() as u64);
            db.insert_out_tree_rows(&parent_rows)?;
            // Clear the accumulators before the next run to avoid huge structures in ram.
            current_parents.clear();
            parent_rows.clear();

            Ok(())
        }
    )
}

/// Given a vectors of StrippedRecords compute the new OutTreeRows. In new-name
/// mode the query is DB-filtered to rows with a valid (len > 0) `new_name`.
fn build_new_out_tree_rows(
    entries: &Vec<StrippedRecord>, root: &Path, sources: Option<(&Path, &Path)>, use_new_name: bool)
    -> Vec<NewOutTreeRow> {
    let mut processed: Vec<NewOutTreeRow> = Vec::new();

    for r in entries {
        let path = if use_new_name {
            // Re-clamp the mangled member so a transform-injected `../` cannot
            // escape the extraction root.
            let (cleaned, _) = strip_leading_up(&Path::new(
                r.new_name.as_deref().expect("filtered to valid new_name")).clean());
            root.join(cleaned)
        } else {
            match sources {
                None => root.join(abs_rel_path(&r.abs_path)),
                Some((source_abs, original_path)) => {
                    root.join(rel_rel_path(source_abs, original_path, &r.abs_path))
                }
            }
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

// -------------------------------------------------------------------------------------------------
// Util
// -------------------------------------------------------------------------------------------------

/// Serialization for Component for better errors.
fn component_to_str<'a>(c: &Component) -> &'a str {
    match c {
        Component::RootDir => "root-dir",
        Component::ParentDir => "parent-dir",
        Component::CurDir => "current-dir",
        Component::Prefix(_) => "prefix", // I think Windows C:!
        Component::Normal(_) => "normal",
    }
}

/// Strip up to `strip` leading components of a relative member path.
/// `0` is a no-op (`None`). A member with `<= strip` components collapses to
/// `Some("")`, matching GNU tar's "transforms to empty name" skip.
fn strip_relative_member(member: &str, strip: u32) -> String {
    if strip == 0 {
        return member.to_string();
    }
    let path = Path::new(member);
    let comps: Vec<&OsStr> = path
        .components()
        .filter_map(|comp| match comp {
            // INFO:
            //  - RootDir -> excluded due to rel paths.
            //  - Prefix -> excluded due to Unix
            //  - CurDir, ParentDir -> Excluded due to .clean()
            Component::Normal(name) => Some(name),
            other => {
                panic!("Unexpected path component {}. Only `Normal` expected",
                       component_to_str(&other)
                );
            }
        })
        .collect();
    if comps.len() as u32 <= strip {
        return String::new();
    }
    comps[strip as usize..]
        .iter()
        .map(|name| name.to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

fn abs_rel_path(abs: &Path) -> PathBuf {
    debug_assert!(abs.is_absolute(), "abs_paths in database MUST be absolute");
    debug_assert_eq!(abs.clean(), abs, "abs_paths in database MUST be normalized");
    abs.strip_prefix("/").expect("abs path starts with /").to_path_buf()
}

/// Relative target of an entry under a relative source: `original_path` (the
/// raw CLI arg) is clamped so leading `../` and a leading `/` can never escape
/// the extraction root, then the within-source remainder is appended.
fn rel_rel_path(source_abs: &Path, original_path: &Path, abs: &Path) -> PathBuf {
    debug_assert!(abs.is_absolute(), "abs_paths in database MUST be absolute");
    debug_assert_eq!(abs.clean(), abs, "abs_paths in database MUST be normalized");
    let san_org = original_path.clean();
    let prefix = if san_org.is_absolute() {
        if san_org == Path::new("/") {
            PathBuf::new()
        } else {
            san_org.strip_prefix("/").expect("absolute expects / at the beginning").to_path_buf()
        }
    } else {
        strip_leading_up(&san_org).0
    };
    let stem = abs.strip_prefix(source_abs).expect("entry must start within source root");
    prefix.join(stem).clean()
}

/// Strip leading ../ in relative paths s.t. they do not escape from the extraction target.
fn strip_leading_up(path: &Path) -> (PathBuf, u64) {
    debug_assert_eq!(path, path.clean(),
                     "INVARIANT ERROR: Function should only work on clean paths");
    let mut components = path.components().peekable();
    let mut ups = 0u64;
    // Consume the components that are pardir.
    while matches!(components.peek(), Some(Component::ParentDir)) {
        components.next();
        ups += 1;
    }
    // Build a path from the remainder
    let mut out = PathBuf::new();
    for comp in components {
        if let Component::Normal(name) = comp {
            out.push(name);
        }
    }
    (out, ups)
}

// ---------------------------------------------------------------------------------------------------------------------
// TEST
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::ExtractConfig;
    use crate::db::flags::{FileFlags, SourceFlag, SourceFlags};
    use crate::db::types::{FilePhase, LinkType, NewFileRecord};
    use crate::test_support::{ExtractWorld, seed_filter_reasons};
    use rusqlite::named_params;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    impl ExtractWorld {
        /// Place-side prologue test world: shared `ExtractWorld` with a
        /// `for_place_test` config (fake extraction root, empty work dir
        /// re-rooted under the tempdir so the DB has a home).
        fn place_prologue(root: &Path, absolute_names: bool, tune: impl FnOnce(&mut ExtractConfig))
            -> Self {
            Self::with_config(|dp| {
                let mut config = ExtractConfig::for_place_test(root.to_path_buf(), absolute_names, 4);
                config.paths.work_dir = dp.join("estage");
                tune(&mut config);
                config
            })
        }

        fn add_source(&self, abs: &str, original: &str) -> i64 {
            self.db.add_get_source(
                Path::new(abs), "test", Some(1), Path::new(original),
                Path::new(abs), Path::new(original),
                SourceFlags::default().with(SourceFlag::IsDirectory, true),
            ).expect("add source")
        }

        fn add_file(&self, source_id: Option<i64>, abs: &str, size: u64, ftype: FileType) -> FileId {
            let rec = NewFileRecord {
                abs_path: PathBuf::from(abs),
                abs_path_raw: None,
                ext: String::new(),
                size,
                mtime: None, atime: None, ctime: None,
                uid: None, gid: None, mode: None,
                ftype: Some(ftype),
                xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None,
                link_dst: None, link_dst_raw: None,
                device_id: None, inode_id: None, major: None, minor: None,
            };
            match source_id {
                Some(sid) => {
                    self.db.insert_file_and_ref(sid, &rec).expect("insert file+ref");
                }
                None => {
                    self.db.insert_file(&rec).expect("insert file");
                }
            }
            let id = self.db.file_id_by_abs_path(Path::new(abs))
                .expect("lookup").expect("row present");
            seed_filter_reasons(&self.db, id);
            id
        }

        /// Seed both `filter_reason_*` tables with the internal include sentinel
        /// (-1) so a `files.include_reason_* = -1` row satisfies the FK.
        fn seed_filter_tables(&self) {
            self.seed_filter();
        }

        /// Raw-SQL `files` insert bypassing `insert_file`'s
        /// `debug_assert!(abs_path.is_absolute())` — lets tests hand the prologue
        /// malformed rows on purpose (debug_assert tripping).
        fn raw_insert(&self, id: i64, abs_path: &str) {
            self.seed_filter_tables();
            self.db.with_transaction(|conn| {
                conn.execute(
                    "INSERT INTO files (id, abs_path, abs_path_raw, ftype, ext, size, \
                     include_reason_archive, include_reason_extract) \
                     VALUES (:id, :abs, :raw, 'file', '', 0, -1, -1)",
                    named_params! { ":id": id, ":abs": abs_path, ":raw": abs_path.as_bytes().to_vec() },
                ).expect("raw insert file");
                Ok(())
            }).expect("raw insert tx");
        }

        fn raw_out_tree(&self) -> Vec<(String, i64, Option<i64>, Option<i64>)> {
            self.db.with_transaction(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT abs_path, flags, file_id, canonical_id FROM out_tree ORDER BY abs_path"
                ).expect("prepare");
                let rows = stmt.query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?,
                        r.get::<_, Option<i64>>(2)?, r.get::<_, Option<i64>>(3)?))
                }).expect("query");
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>().expect("collect"))
            }).expect("read out_tree")
        }
    }

    /// Minimal `StrippedRecord` for the pure `build_new_out_tree_rows` tests.
    fn mk_stripped(id: i64, abs: &str, ftype: FileType, new_name: Option<&str>) -> StrippedRecord {
        StrippedRecord {
            id: FileId(id),
            abs_path: PathBuf::from(abs),
            abs_path_raw: None,
            ext: String::new(),
            size: 0,
            sha1: None,
            mtime: None, atime: None, ctime: None,
            ftype,
            device_id: None, inode_id: None,
            canonical_id: None,
            flags: FileFlags::default(),
            phase: FilePhase::Unarchived,
            new_name: new_name.map(str::to_string),
        }
    }

    fn all_file_types() -> Vec<FileType> {
        vec![
            FileType::File,
            FileType::Directory,
            FileType::Socket,
            FileType::FIFO,
            FileType::CharacterDevice,
            FileType::BlockDevice,
            FileType::Unknown,
            FileType::Symlink(LinkType::File),
            FileType::Symlink(LinkType::Directory),
            FileType::Symlink(LinkType::Socket),
            FileType::Symlink(LinkType::FIFO),
            FileType::Symlink(LinkType::CharacterDevice),
            FileType::Symlink(LinkType::BlockDevice),
            FileType::Symlink(LinkType::Dangling),
            FileType::Symlink(LinkType::Cycle),
            FileType::Symlink(LinkType::Unknown),
        ]
    }

    // ---- resolve_transform --------------------------------------------------

    #[test]
    fn resolve_transform_none_is_none() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), false, |_| {});
        assert!(resolve_transform(&world.rt()).expect("resolve").is_none());
    }

    #[test]
    fn resolve_transform_cli_parses() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), false, |c| {
            c.transform_policy = TransformSource::Cli("s,a,b,".to_string());
        });
        let t = resolve_transform(&world.rt()).expect("resolve").expect("some");
        assert_eq!(t.apply("a/c.txt"), "b/c.txt");
    }

    #[test]
    fn resolve_transform_cli_bad_expr_errors() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), false, |c| {
            c.transform_policy = TransformSource::Cli("garbage".to_string());
        });
        assert!(resolve_transform(&world.rt()).is_err());
    }

    #[test]
    fn resolve_transform_stored_reads_meta() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), false, |c| {
            c.transform_policy = TransformSource::Stored;
        });
        world.db.set_archive_transform("s,a,b,").expect("set meta");
        let t = resolve_transform(&world.rt()).expect("resolve").expect("some");
        assert_eq!(t.apply("a/c.txt"), "b/c.txt");
    }

    #[test]
    fn resolve_transform_stored_without_meta_is_none() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), false, |c| {
            c.transform_policy = TransformSource::Stored;
        });
        assert!(resolve_transform(&world.rt()).expect("resolve").is_none());
    }

    // ---- populate_new_names (debug_assert trips) ----------------------------

    #[test]
    fn populate_new_names_panics_on_relative_abs_path() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), true, |_| {});
        world.raw_insert(1, "relative/x");
        let result = catch_unwind(AssertUnwindSafe(|| populate_new_names(&world.rt(), None)));
        assert!(result.is_err(), "abs_rel_path must reject relative abs_paths");
    }

    #[test]
    fn populate_new_names_panics_on_unclean_abs_path() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), true, |_| {});
        // `..` is resolved by clean() but not by Path::eq -> assert trips.
        // (`/a/./b` would NOT: Path::eq normalizes `.` away like clean does.)
        world.raw_insert(1, "/a/b/../c");
        let result = catch_unwind(AssertUnwindSafe(|| populate_new_names(&world.rt(), None)));
        assert!(result.is_err(), "abs_rel_path must reject non-normalized abs_paths");
    }

    // ---- strip_relative_member ----------------------------------------------

    #[test]
    fn strip_relative_member_counts_components() {
        assert_eq!(strip_relative_member("a/b/c.txt", 1), "b/c.txt");
        assert_eq!(strip_relative_member("a/b/c.txt", 2), "c.txt");
        assert_eq!(strip_relative_member("a/b/c.txt", 3), "");
        assert_eq!(strip_relative_member("a/b/c.txt", 9), "");
    }

    #[test]
    fn strip_relative_member_zero_is_noop() {
        assert_eq!(strip_relative_member("a/b", 0), "a/b");
    }

    /// Only `Normal` components are legal in a relative member; anything else
    /// (root dir, `..`, `.`) panics with a descriptive message.
    #[test]
    fn strip_relative_member_panics_on_absolute_path() {
        let err = catch_unwind(AssertUnwindSafe(|| strip_relative_member("/a/b", 1)))
            .expect_err("absolute member must panic");
        let msg = err.downcast_ref::<String>().expect("panic payload");
        assert!(msg.contains("root-dir"), "got: {msg}");
    }

    #[test]
    fn strip_relative_member_panics_on_parent_dir() {
        let err = catch_unwind(AssertUnwindSafe(|| strip_relative_member("a/../b", 1)))
            .expect_err("`..` member must panic");
        let msg = err.downcast_ref::<String>().expect("panic payload");
        assert!(msg.contains("parent-dir"), "got: {msg}");
    }

    #[test]
    fn strip_relative_member_panics_on_cur_dir() {
        let err = catch_unwind(AssertUnwindSafe(|| strip_relative_member("./a", 1)))
            .expect_err("leading `.` member must panic");
        let msg = err.downcast_ref::<String>().expect("panic payload");
        assert!(msg.contains("current-dir"), "got: {msg}");
    }

    /// GNU order: transform is applied first, then the required number of
    /// components is stripped from its result. Mirrors `apply_renames`.
    #[test]
    fn transform_then_strip_order() {
        let t = parse_transform_expr("s,^a/,b/,").unwrap();
        let member = "a/x/c.txt";
        let transformed = t.apply(member);
        assert_eq!(transformed, "b/x/c.txt");
        assert_eq!(strip_relative_member(&transformed, 1), "x/c.txt");
    }

    // ---- abs/rel/new-name path helpers --------------------------------------

    #[test]
    fn abs_rel_path_drops_leading_slash() {
        assert_eq!(abs_rel_path(Path::new("/a/b.txt")), PathBuf::from("a/b.txt"));
        assert_eq!(abs_rel_path(Path::new("/")), PathBuf::from(""));
    }

    #[test]
    fn rel_rel_path_builds_clamped_target() {
        let src = Path::new("/home/user/Movies");
        // leading `..` in the original path is clamped away (can't escape `<ed>`)
        let abs = Path::new("/home/user/Movies/Best-Movie-Ever.mp4");
        assert_eq!(
            rel_rel_path(src, Path::new("../../Movies"), abs),
            PathBuf::from("Movies/Best-Movie-Ever.mp4"),
        );
        // absolute original path drops its leading `/`
        assert_eq!(
            rel_rel_path(src, Path::new("/home/user/Movies"), abs),
            PathBuf::from("home/user/Movies/Best-Movie-Ever.mp4"),
        );
        // `/` root original path -> empty prefix
        assert_eq!(
            rel_rel_path(src, Path::new("/"), Path::new("/home/user/Movies/sub/x.mp4")),
            PathBuf::from("sub/x.mp4"),
        );
    }

    #[test]
    fn strip_leading_up_counts_and_clamps() {
        assert_eq!(strip_leading_up(Path::new("../../a/b")), (PathBuf::from("a/b"), 2));
        assert_eq!(strip_leading_up(Path::new("../x")), (PathBuf::from("x"), 1));
        assert_eq!(strip_leading_up(Path::new("a/b")), (PathBuf::from("a/b"), 0));
        assert_eq!(strip_leading_up(Path::new("..")), (PathBuf::from(""), 1));
    }

    #[test]
    fn strip_leading_up_clamps_after_clean() {
        // The re-clamp path used by the out_tree builder normalizes first.
        let clamp = |name: &str| strip_leading_up(&Path::new(name).clean()).0;
        assert_eq!(clamp("Movies/Best-Movie-Ever.mp4"), PathBuf::from("Movies/Best-Movie-Ever.mp4"));
        assert_eq!(clamp("../Movies/x.mp4"), PathBuf::from("Movies/x.mp4"));
        assert_eq!(clamp("../../.."), PathBuf::from(""));
        assert_eq!(clamp("/abs/x"), PathBuf::from("abs/x"));
    }

    /// Order lock: a transform anchored on the source prefix must see the FULL
    /// relative target (including the clamped prefix), matching the rel mapping.
    #[test]
    fn transform_sees_full_relative_path() {
        let t = parse_transform_expr("s,^Movies,Pics,").unwrap();
        let rel = rel_rel_path(
            Path::new("/home/user/Movies"),
            Path::new("../../Movies"),
            Path::new("/home/user/Movies/Best-Movie-Ever.mp4"),
        );
        let mangled = t.apply(&rel.to_string_lossy());
        assert_eq!(mangled, "Pics/Best-Movie-Ever.mp4");
        // and the out_tree path re-cleans the result.
        let final_path = PathBuf::from("/ed")
            .join(strip_leading_up(&Path::new(&mangled).clean()).0);
        assert_eq!(final_path, PathBuf::from("/ed/Pics/Best-Movie-Ever.mp4"));
    }

    // ---- apply_renames ------------------------------------------------------

    #[test]
    fn apply_renames_writes_new_name_no_transform() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), true, |_| {});
        world.add_file(None, "/a/x/one.txt", 1, FileType::File);
        world.add_file(None, "/a/x/two.txt", 2, FileType::File);
        let entries = world.db.list_materialized_entries::<StrippedRecord>(
            FileId(0), 100, None, None, false, true,
        ).expect("materialized");
        apply_renames(&world.rt(), &entries, None, |abs: &Path| abs_rel_path(abs))
            .expect("rename");
        let one = world.db.get_file_by_id::<StrippedRecord>(FileId(1))
            .expect("get").expect("row");
        assert_eq!(one.new_name.as_deref(), Some("a/x/one.txt"));
    }

    #[test]
    fn apply_renames_applies_transform_then_strip() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), true, |c| {
            c.strip_components = 1;
        });
        world.add_file(None, "/a/x/two.txt", 2, FileType::File);
        let entries = world.db.list_materialized_entries::<StrippedRecord>(
            FileId(0), 100, None, None, false, true,
        ).expect("materialized");
        let t = parse_transform_expr("s,^a,b,").unwrap();
        apply_renames(&world.rt(), &entries, Some(&t), |abs: &Path| abs_rel_path(abs))
            .expect("rename");
        let two = world.db.get_file_by_id::<StrippedRecord>(FileId(1))
            .expect("get").expect("row");
        assert_eq!(two.new_name.as_deref(), Some("x/two.txt"));
    }

    // ---- populate_out_tree_abs / rel ----------------------------------------

    #[test]
    fn populate_out_tree_abs_maps_and_leaves_ref_out_empty() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), true, |_| {});
        world.add_file(None, "/var/docker/cache/one.txt", 1, FileType::File);
        world.add_file(None, "/var/docker/cache/two.txt", 2, FileType::File);
        populate_out_tree_abs(&world.rt(), false).expect("abs out tree");
        let rows = world.raw_out_tree();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|(p, _, _, _)| p == "/ed/var/docker/cache/one.txt"));
        assert!(rows.iter().any(|(p, _, _, _)| p == "/ed/var/docker/cache/two.txt"));
        assert_eq!(world.db.count_ref_out_rows().expect("count"), 0);
    }

    #[test]
    fn populate_out_tree_rel_clamps_and_populates_ref_out() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), false, |_| {});
        let sid = world.add_source("/home/user/Movies", "../../Movies");
        world.add_file(Some(sid), "/home/user/Movies/Best-Movie-Ever.mp4", 3, FileType::File);
        world.add_file(Some(sid), "/home/user/Movies/sub/x.mp4", 4, FileType::File);
        populate_out_tree_rel(&world.rt(), false).expect("rel out tree");
        let rows = world.raw_out_tree();
        assert_eq!(rows.len(), 2);
        // leading `..` of the original path is clamped away.
        assert!(rows.iter().any(|(p, _, _, _)| p == "/ed/Movies/Best-Movie-Ever.mp4"));
        assert!(rows.iter().any(|(p, _, _, _)| p == "/ed/Movies/sub/x.mp4"));
        assert_eq!(world.db.count_ref_out_rows().expect("count"), 2);
    }

    /// `--files-from` file whose parent is not covered by any source: the
    /// `ensure_parent` step of `populate_out_tree` must mint the missing dir rows.
    #[test]
    fn populate_out_tree_ensures_parents_for_orphan_file() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), false, |_| {});
        let sid = world.add_source("/x", "./x");
        world.add_file(Some(sid), "/x/y/z.txt", 5, FileType::File);
        populate_out_tree(&world.rt(), false).expect("out tree");
        let rows = world.raw_out_tree();
        let file_row = rows.iter()
            .find(|(p, _, _, _)| p == "/ed/x/y/z.txt")
            .expect("file row present");
        assert!(file_row.2.is_some(), "file row must carry its file_id");
        assert!(rows.iter().any(|(p, flags, fid, _)| {
            p == "/ed/x/y"
                && fid.is_none()
                && (flags & OutTreeFlag::IsDirectory.mask_i64()) != 0
        }), "parent `/ed/x/y` must be minted as a dir row");
    }

    // ---- build_new_out_tree_rows --------------------------------------------

    #[test]
    fn build_new_out_tree_rows_clamps_mangled_new_names() {
        let root = Path::new("/ed");
        let entries = vec![
            mk_stripped(1, "/a/b.txt", FileType::File, Some(".././../evil.txt")),
            mk_stripped(2, "/etc/passwd", FileType::File, Some("/etc/passwd")),
            mk_stripped(3, "/x", FileType::File, Some("../../..")),
        ];
        let rows = build_new_out_tree_rows(&entries, root, None, true);
        let paths: Vec<String> = rows.iter()
            .map(|r| r.abs_path.to_string_lossy().into_owned())
            .collect();
        assert_eq!(paths, vec!["/ed/evil.txt", "/ed/etc/passwd", "/ed/"]);
    }

    #[test]
    fn build_new_out_tree_rows_dir_flag_only_for_directories() {
        let root = Path::new("/ed");
        for (i, ftype) in all_file_types().into_iter().enumerate() {
            let entries = vec![mk_stripped(i as i64 + 1, "/x", ftype, None)];
            let rows = build_new_out_tree_rows(&entries, root, None, false);
            assert_eq!(rows.len(), 1);
            let is_dir = rows[0].flags.get(OutTreeFlag::IsDirectory);
            assert_eq!(is_dir, ftype == FileType::Directory,
                       "IsDirectory for {ftype:?} must be {}", ftype == FileType::Directory);
        }
    }

    // ---- prepare_hardlink_canonicals ----------------------------------------

    #[test]
    fn prepare_hardlink_canonicals_none_marks_all_files_self_canonical() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), true, |c| {
            c.placement.hard_link_grouping = HardLinkGrouping::None;
        });
        let f1 = world.add_file(None, "/a/one.txt", 1, FileType::File);
        world.add_file(None, "/a/dir", 0, FileType::Directory);
        let entries = world.db.list_materialized_entries::<StrippedRecord>(
            FileId(0), 100, None, None, false, true,
        ).expect("materialized");
        let rows = build_new_out_tree_rows(&entries, Path::new("/ed"), None, false);
        world.db.insert_out_tree_rows(&rows).expect("insert out tree");
        prepare_hardlink_canonicals(&world.rt()).expect("canonical election");

        let ot = world.raw_out_tree();
        let file_row = ot.iter().find(|(p, _, _, _)| p == "/ed/a/one.txt").expect("file row");
        // out_tree ids follow insertion order: the file row is id 1.
        assert_eq!(file_row.3, Some(f1.0 as i64), "file row must be self-canonical");
        let dir_row = ot.iter().find(|(p, _, _, _)| p == "/ed/a/dir").expect("dir row");
        assert_eq!(dir_row.3, None, "dir rows are never canonical");
    }

    // ---- stepped_dirs_then_rest ---------------------------------------------

    #[test]
    fn stepped_dirs_then_rest_orders_dirs_first_and_visits_all() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), false, |_| {});
        let sid = world.add_source("/t", "./t");
        let dir = world.add_file(Some(sid), "/t/sub", 0, FileType::Directory);
        let f1 = world.add_file(Some(sid), "/t/a.txt", 1, FileType::File);
        let sl = world.add_file(Some(sid), "/t/link", 0, FileType::Symlink(LinkType::File));
        let unk = world.add_file(Some(sid), "/t/u", 0, FileType::Unknown);

        let mut seen: Vec<(i64, FileType)> = Vec::new();
        let mut collect = |entries: &Vec<StrippedRecord>| -> Result<()> {
            for e in entries {
                seen.push((e.id.0, e.ftype));
            }
            Ok(())
        };
        stepped_dirs_then_rest(&world.rt(), Some(sid), false, &mut collect).expect("stepped");

        let expected = vec![
            (dir.0 as i64, FileType::Directory),
            (f1.0 as i64, FileType::File),
            (sl.0 as i64, FileType::Symlink(LinkType::File)),
            (unk.0 as i64, FileType::Unknown),
        ];
        assert_eq!(seen, expected, "dirs first, then the rest, each exactly once");
    }

    // ---- run() smoke + idempotency ------------------------------------------

    #[test]
    fn run_builds_out_tree_and_is_idempotent() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), false, |c| {
            c.transform_policy = TransformSource::Cli("s,^Movies,Pics,".to_string());
            c.strip_components = 1;
        });
        let sid = world.add_source("/home/user/Movies", "../../Movies");
        world.add_file(Some(sid), "/home/user/Movies/Best-Movie-Ever.mp4", 3, FileType::File);
        run(&world.rt()).expect("prologue");

        assert!(world.db.placement_prologue_done().expect("done"));
        assert!(world.db.out_tree_is_built().expect("built"));
        // new_name = rel("Movies/Best-Movie-Ever.mp4") -> transform ^Movies=Pics
        // -> strip 1 -> "Best-Movie-Ever.mp4" -> re-clamped under `<ed>`.
        let ot = world.raw_out_tree();
        assert!(ot.iter().any(|(p, _, _, _)| p == "/ed/Best-Movie-Ever.mp4"));
        assert_eq!(world.db.count_ref_out_rows().expect("count"), 1);
        let n = world.db.count_out_tree_rows().expect("count");

        // Resume: the `placement_prologue_done` gate short-circuits.
        run(&world.rt()).expect("second run");
        assert_eq!(world.db.count_out_tree_rows().expect("count"), n);
    }

    // ---- one_top_level -------------------------------------------------------

    #[test]
    fn one_top_level_prefixes_abs_out_tree() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), true, |c| {
            c.placement.one_top_level = Some(PathBuf::from("otl"));
        });
        world.add_file(None, "/var/cache/one.txt", 1, FileType::File);
        populate_out_tree_abs(&world.rt(), false).expect("abs out tree");
        let rows = world.raw_out_tree();
        assert_eq!(rows.len(), 1);
        assert!(rows.iter().any(|(p, _, _, _)| p == "/ed/otl/var/cache/one.txt"));
    }

    #[test]
    fn one_top_level_prefixes_rel_out_tree() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), false, |c| {
            c.placement.one_top_level = Some(PathBuf::from("otl"));
        });
        let sid = world.add_source("/home/user/Movies", "../../Movies");
        world.add_file(Some(sid), "/home/user/Movies/Best-Movie-Ever.mp4", 3, FileType::File);
        populate_out_tree_rel(&world.rt(), false).expect("rel out tree");
        let rows = world.raw_out_tree();
        assert_eq!(rows.len(), 1);
        assert!(rows.iter().any(|(p, _, _, _)| p == "/ed/otl/Movies/Best-Movie-Ever.mp4"));
    }

    #[test]
    fn one_top_level_run_maps_under_subdir() {
        let world = ExtractWorld::place_prologue(Path::new("/ed"), false, |c| {
            c.placement.one_top_level = Some(PathBuf::from("otl"));
            c.transform_policy = TransformSource::Cli("s,^Movies,Pics,".to_string());
            c.strip_components = 1;
        });
        let sid = world.add_source("/home/user/Movies", "../../Movies");
        world.add_file(Some(sid), "/home/user/Movies/Best-Movie-Ever.mp4", 3, FileType::File);
        run(&world.rt()).expect("prologue");
        let ot = world.raw_out_tree();
        assert!(ot.iter().any(|(p, _, _, _)| p == "/ed/otl/Best-Movie-Ever.mp4"));
    }
}
