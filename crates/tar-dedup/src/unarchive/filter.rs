//! Extract filter phase: pure DB pass applying the user's include/exclude rules
//! against `files.abs_path`, writing `include_reason_extract` / `exclude_reason_extract`.

use crate::common::filter::ingest_filters as internal_ingest_filter;
use crate::common::filter::{FilterSink, joint_filter_phase};
use crate::config::ExtractConfig;
use crate::db::types::FilePhase;
use crate::db::types::FilePhase::Unarchived;
use crate::db::{ErrorPhase, Recorder};
use crate::error::Result;
use crate::unarchive::ExtractRTArgs;
use std::cell::RefCell;

const ERROR_PHASE: ErrorPhase = ErrorPhase::Extract(crate::config::ExtractPipelinePhase::Filter);

#[derive(Clone, Debug, Default)]
pub struct ParseFilterBuffer {
    /// Content is `from`, `line`, `query`
    include_filters: Vec<(String, Option<u64>, String)>,
    /// Content is `from`, `line`, `query`
    exclude_filters: Vec<(String, Option<u64>, String)>,
}

/// When extracting, it is not guaranteed that a database is present. This buffer captures the
/// parsed filter results and stores it until it is written to the db. This allows for validation
/// at invocation time rather when the database becomes available.
impl ParseFilterBuffer {
    pub fn add_include(&mut self, from: &str, line: Option<u64>, query: &str) -> Result<u64> {
        self.include_filters.push((from.to_string(), line, query.to_string()));
        Ok(1)
    }
    pub fn add_exclude(&mut self, from: &str, line: Option<u64>, query: &str) -> Result<u64> {
        self.exclude_filters.push((from.to_string(), line, query.to_string()));
        Ok(1)
    }
    pub fn count_includes(&self) -> u64 {
        self.include_filters.len() as u64
    }

    pub fn write_to_db<'a>(
        &self,
        db_add_include: Box<dyn Fn(&str, Option<u64>, &str) -> Result<u64> + 'a>,
        db_add_exclude: Box<dyn Fn(&str, Option<u64>, &str) -> Result<u64> + 'a>)
        -> Result<u64> {

        for include in self.include_filters.iter() {
            db_add_include(&include.0, include.1, &include.2)?;
        }
        for exclude in self.exclude_filters.iter() {
            db_add_exclude(&exclude.0, exclude.1, &exclude.2)?;
        }

        Ok((self.include_filters.len() + self.exclude_filters.len()) as u64)
    }
}

// INFO: When no manifest DB was found (truncated / non-conform archive), the extract
//  filter pass is a deliberate no-op: there is nothing to match against, and filters
//  stay in `ExtractConfig`. Verified via the row count.
pub fn run(rt: &ExtractRTArgs, eager_filter: bool) -> Result<()> {
    // TODO:
    //  - option should exist to provide the progress bars separately. (Extra progress bars when
    //    extracting during scan)
    //  - Phase should only be set in during the actual filter phase
    if !rt.db.get_extract_filters_applied()? {
        joint_filter_phase(
            false,
            rt.config.filter.anchored,
            rt.config.filter.ignore_case,
            rt.config.filter.no_unicode,
            rt.config.process.batch_size,
            Unarchived,
            &rt.db,
            rt.progress,
            |is_exclude| rt.db.count_filters_extract(is_exclude),
            |is_exclude| rt.db.get_filters_extract(is_exclude),
            || rt.db.apply_no_filter_extract(),
            |lid, batch_size| rt.db.get_rows_to_filter_extract(
                *lid, batch_size
            ),
            |results| rt.db.apply_filter_result_extract(results.into_iter())
        )?;
        rt.db.set_extract_filters_applied(true)?;
    }
    if !eager_filter {
        let updated = rt.db.global_mark_phase(FilePhase::ExtractFiltered)?;
        tracing::info!("Promoted {updated} entries to ExtractFiltered");
    }
    Ok(())
}

pub fn ingest_filters(config: &ExtractConfig, recorder: &mut Recorder) -> Result<ParseFilterBuffer> {
    // INFO: The sink closures may not borrow `pbf` mutably (they are `Fn`, and two
    //  closures would conflict over `&mut`), so interior mutability via `RefCell`.
    let pbf = RefCell::new(ParseFilterBuffer::default());
    internal_ingest_filter(
        &config.filter.include_patterns,
        &config.filter.include_from,
        &config.filter.exclude_patterns,
        &config.filter.exclude_from,
        config.filter.ignore_case,
        config.filter.no_unicode,
        config.process.fail_fast,
        recorder,
        ERROR_PHASE,
        FilterSink {
            add_include: Box::new(|from, line, query| {
                pbf.borrow_mut().add_include(from, line, query)
            }),
            add_exclude: Box::new(|from, line, query| {
                pbf.borrow_mut().add_exclude(from, line, query)
            }),
            count_includes: Box::new(|| {
                Ok(pbf.borrow().count_includes())
            }),
        },
    )?;
    // All sink borrows are released here; unwrap the buffer for the scan stage.
    Ok(pbf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::files::original_extension;
    use crate::db::Database;
    use crate::db::types::{FileId, FileType, NewFileRecord};
    use crate::progress::{EXTRACT_MULTIPLIER, ProgressBarSet};
    use crate::shutdown::Shutdown;
    use crate::unarchive::ExtractRTArgs;

    struct ExtractTestWorld {
        dir: tempfile::TempDir,
        db: Database,
        shutdown: Shutdown,
        progress: ProgressBarSet,
        config: ExtractConfig,
    }

    impl ExtractTestWorld {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let work_dir = dir.path().join("estage");
            std::fs::create_dir_all(&work_dir).expect("create work dir");
            let config = ExtractConfig::for_scan_test(
                dir.path().join("archive.tar"),
                work_dir,
                dir.path().join("out"),
            );
            let db = Database::open(&config.paths.db_path()).expect("open db");
            Self {
                dir,
                db,
                shutdown: Shutdown::detached(),
                progress: ProgressBarSet::new(EXTRACT_MULTIPLIER),
                config,
            }
        }

        fn rt(&self) -> ExtractRTArgs<'_> {
            ExtractRTArgs {
                config: &self.config,
                db: &self.db,
                shutdown: &self.shutdown,
                progress: &self.progress,
            }
        }

        fn seed_internal_include(&self) {
            self.db.with_transaction(|conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO filter_reason_extract (id, source, line, expression) \
                     VALUES (-1, 'internal', NULL, '.*')",
                    [],
                ).expect("seed");
                Ok(())
            }).expect("seed tx");
        }

        fn add_unarchived(&self, name: &str) -> FileId {
            let p = self.dir.path().join(name);
            self.db.insert_file(&NewFileRecord {
                abs_path: p.clone(),
                ext: original_extension(&p),
                size: 1,
                mtime: None, atime: None, ctime: None,
                uid: None, gid: None, mode: None,
                ftype: Some(FileType::File),
                xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None, link_dst: None, abs_path_raw: None, link_dst_raw: None,
                device_id: None, inode_id: None, major: None, minor: None,
            }).expect("insert");
            let id = self.db.file_id_by_abs_path(&p).expect("lookup").expect("row");
            self.db.mark_file_phase(id, FilePhase::Unarchived).expect("phase");
            id
        }
    }

    fn extract_reason_pairs(world: &ExtractTestWorld) -> Vec<(i64, i64)> {
        world.db.with_transaction(|conn| {
            let mut stmt = conn.prepare(
                "SELECT include_reason_extract, exclude_reason_extract FROM files ORDER BY id",
            ).expect("stmt");
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            }).expect("query").collect::<rusqlite::Result<Vec<_>>>().expect("collect");
            Ok(rows)
        }).expect("tx")
    }

    fn seed_user_filters(world: &ExtractTestWorld) -> ParseFilterBuffer {
        let mut rec = Recorder::speculative(
            !world.config.process.no_errors, Some(world.config.process.auto_flush_limit));
        let buf = ingest_filters(&world.config, &mut rec).expect("ingest");
        buf.write_to_db(
            Box::new(
                |from, line, query| world.db.add_include_pattern_extract(
                    from, line, query)),
            Box::new(
                |from, line, query| world.db.add_exclude_pattern_extract(
                    from, line, query)),
        ).expect("write");
        buf
    }

    #[test]
    fn ingest_filters_buffer_and_write_to_db() {
        let mut world = ExtractTestWorld::new();
        world.config.filter.include_patterns = vec!["keep".into()];
        world.config.filter.exclude_patterns = vec!["skip".into()];

        let buf = seed_user_filters(&world);

        // The pre-DB buffer captured both arms verbatim.
        assert_eq!(buf.include_filters, vec![("--include".into(), Some(0), "keep".into())]);
        assert_eq!(buf.exclude_filters, vec![("--exclude".into(), Some(0), "skip".into())]);
        // ...and write_to_db landed them in the extract tables.
        assert_eq!(world.db.count_filters_extract(Some(false)).expect("c"), 1);
        assert_eq!(world.db.count_filters_extract(Some(true)).expect("c"), 1);
        let inc = world.db.get_filters_extract(false).expect("get");
        assert_eq!(inc[0].expression, "keep");
    }

    #[test]
    fn ingest_filters_empty_buffer_appends_internal_catch_all() {
        let world = ExtractTestWorld::new();
        let mut rec = Recorder::speculative(
            !world.config.process.no_errors, Some(world.config.process.auto_flush_limit));
        let buf = ingest_filters(&world.config, &mut rec).expect("ingest");
        // No user patterns -> the internal `.*` catch-all rides in the buffer.
        assert_eq!(buf.include_filters, vec![("internal".into(), None, ".*".into())]);
        assert!(buf.exclude_filters.is_empty());
    }

    #[test]
    fn run_fast_path_promotes_all() {
        let world = ExtractTestWorld::new();
        world.seed_internal_include();
        world.add_unarchived("a");
        world.add_unarchived("b");

        run(&world.rt(), false).expect("run");

        assert_eq!(extract_reason_pairs(&world), vec![(-1, 0), (-1, 0)]);
        assert_eq!(world.db.count_files_in_phase(FilePhase::ExtractFiltered).expect("c"), 2);
    }

    #[test]
    fn run_with_user_filters_sets_reasons_and_promotes() {
        let mut world = ExtractTestWorld::new();
        world.config.filter.include_patterns = vec!["keep".into()];
        world.config.filter.exclude_patterns = vec!["skip".into()];
        seed_user_filters(&world);
        // Fresh extract catalog holds only the id-0 dummy -> first include takes -1,
        // first exclude takes +1 (matches production).
        world.add_unarchived("keep_a");
        world.add_unarchived("keep_skip_b");
        world.add_unarchived("skip_c");
        world.add_unarchived("other_d");

        run(&world.rt(), false).expect("run");

        assert_eq!(extract_reason_pairs(&world), vec![(-1, 0), (-1, 1), (0, 1), (0, 0)]);
        assert_eq!(world.db.count_files_in_phase(FilePhase::ExtractFiltered).expect("c"), 4);
        assert_eq!(world.db.count_files_in_phase(FilePhase::Unarchived).expect("c"), 0);
    }
}