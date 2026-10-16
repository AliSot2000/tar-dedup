//! Shared filter application used by both the archive and extract pipelines.

use crate::common::batched_stepped_loop;
use crate::db::flags::ErrorFlags;
use crate::db::types::{FileId, FilePhase, FilterExpression, StrippedRecord};
use crate::db::{Database, ErrorPhase, Recorder};
use crate::error::{Error, FileStatError, Result};
use crate::progress::{BarKind, ProgressBarSet};
use indicatif::ProgressBar;
use regex::bytes::{Regex, RegexBuilder};
use std::fs;
use std::path::PathBuf;

/// A filter expression compiled to a regex for fast matching.
pub struct ParsedFilter {
    pub id: i64,
    pub expression: Regex,
}

/// Outcome of matching a single record against the include/exclude sets.
/// INFO: Include ids are negative; exclude ids are positive.
pub struct FilterResult {
    pub id: FileId,
    pub include_reason: i64,
    pub exclude_reason: i64,
}

pub fn joint_filter_phase(
    update_hardlink_detection: bool,
    anchored: bool,
    ignore_case: bool,
    no_unicode: bool,
    batch_size: u64,
    pv: FilePhase,
    db: &Database,
    pg: &ProgressBarSet,
    // Closures to stand in for the db
    count_filters: impl Fn(Option<bool>) -> Result<u64>,
    get_filters: impl Fn(bool) -> Result<Vec<FilterExpression>>,
    apply_no_filter: impl Fn() -> Result<u64>,
    get_rows_to_filter: impl Fn(&FileId, u64) -> Result<Vec<StrippedRecord>>,
    apply_results: impl Fn(Vec<(FileId, i64, i64)>) -> Result<u64>)
    -> Result<()> {

    let already_filtered = db.count_files_in_phase(pv)?;
    let total = db.count_entries()?;
    pg.set_phase_total(total);
    pg.inc_both(already_filtered);

    let include_count = count_filters(Some(false))?;
    let exclude_count = count_filters(Some(true))?;

    // Handle case when nothing is
    if include_count == 1 && exclude_count == 0 {
        let include_filter = &get_filters(false)?[0];
        if include_filter.is_internal() {
            debug_assert_eq!(
                include_filter.expression, ".*",
                "Unexpected Filter expression. Filtering perhaps not working correctly?"
            );

            let updated = apply_no_filter()?;
            pg.inc_global(updated);
            tracing::info!("No filters present. All {updated} files selected.");
            debug_assert_eq!(total, updated, "Updated rows and total rows don't match");
            pg.inc_both(total);
            return Ok(());
        }
    };
    // Perform actual process of filtering. In case this is a noticeable bottleneck, it is a
    // separate function so we can swap in a rayon pool or a crossbeam ... whatever is better.
    step_filters(
        anchored, ignore_case, no_unicode, batch_size, &pv, db, pg, count_filters, get_filters,
        get_rows_to_filter, apply_results
    )?;

    if update_hardlink_detection {
        let (down, up) = db.fix_up_canonical_flag()?;
        assert_eq!(down, up, "Number of clusters with downgrades did not match numbers with upgrade");
    }
    // Archive-only, like the equivalent guard inside `step_filters`: extract rows
    // stay in `pv` until the post-pass global promotion (`set_phase`).
    if pv.is_archive_phase() {
        assert_eq!(0, db.count_files_in_phase(pv)?, "Files left over after the filtering");
    }
    Ok(())
}

pub fn step_filters(
    anchored: bool,
    ignore_case: bool,
    no_unicode: bool,
    batch_size: u64,
    pv: &FilePhase,
    db: &Database,
    pg: &ProgressBarSet,

    // Closures to stand in for the db
    count_filters: impl Fn(Option<bool>) -> Result<u64>,
    get_filters: impl Fn(bool) -> Result<Vec<FilterExpression>>,
    get_rows_to_filter: impl Fn(&FileId, u64) -> Result<Vec<StrippedRecord>>,
    apply_results: impl Fn(Vec<(FileId, i64, i64)>) -> Result<u64>)
    -> Result<()> {

    // Filter parsing
    let add_flt = pg.push_sub_bar(
        "Loading Filters from the DB...", BarKind::Count
    );
    add_flt.set_length(count_filters(Some(true))? + count_filters(Some(false))?);

    let include_filters = parse_filter(
        &get_filters(false)?,
        "include",
        anchored,
        ignore_case,
        no_unicode,
        Some(&add_flt));
    let exclude_filters = parse_filter(
        &get_filters(true)?,
        "exclude",
        anchored,
        ignore_case,
        no_unicode,
        Some(&add_flt));

    drop(add_flt);

    // Perform filtering
    let mut included = 0u64;
    let mut exclude_by_include = 0u64;
    let mut exclude_by_exclude = 0u64;

    batched_stepped_loop(
        batch_size,
        || FileId(0),
        |lid, batch_size| get_rows_to_filter (
            &lid,  batch_size
        ),
        |last: &StrippedRecord| last.id,

        |batch| {
            let processed = batch
                .iter()
                .map(|rec| {
                    let filt_res = test_match(&include_filters, &exclude_filters, rec);
                    if filt_res.exclude_reason == 0 && filt_res.include_reason < 0 {
                        tracing::debug!("Including {}", rec.abs_path.display());
                        included += 1;
                    } else if filt_res.exclude_reason > 0 && filt_res.include_reason < 0 {
                        tracing::debug!("Excluding by Exclude Filter {}", rec.abs_path.display());
                        exclude_by_exclude += 1;
                    } else if filt_res.include_reason == 0  {
                        tracing::debug!("Excluding by Include Filter {}", rec.abs_path.display());
                        exclude_by_include += 1;
                    }
                    assert!(filt_res.exclude_reason >= 0 && filt_res.include_reason <= 0,
                            "INVARIANT ERROR: Exclude Include Reason Violate constrinats.");
                    pg.inc_both(1);
                    filt_res
                })
                .map(|fr| (fr.id, fr.include_reason, fr.exclude_reason)).collect();
            let updated = apply_results(processed)?;
            assert_eq!(
                updated, batch.len() as u64,
                "INVARIANT ERROR: Number of rows updated does not match rows queried. \
                   Rows vanished?"
            );
            pg.inc_both(updated);
            Ok(())
        }
    )?;

    let total = db.count_entries()?;
    tracing::info!("Filtered {total} entries, deemed: {included} included, {exclude_by_include} \
        excluded by include filter, {exclude_by_exclude} excluded by exclude filter.");
    // The zero-leftover invariant only holds for the archive pipeline, where the
    // apply closure advances `phase`; extract rows stay in `pv` (Unarchived) until
    // the global promotion that follows the pass.
    if pv.is_archive_phase() {
        let rem = db.count_files_in_phase(*pv)?;
        assert_eq!(0, rem, "INVARIANT ERROR: {rem} files in previous phase. Zero expected.");
    }
    Ok(())
}


/// Convert filter expressions to compiled regexes, applying `--anchored` and
/// `--ignore-case`.
/// Strong invariants assumed, violation will lead to panics.
pub fn parse_filter(
    filters: &[FilterExpression],
    operation: &str,
    anchored: bool,
    ignore_case: bool,
    no_unicode: bool,
    cp: Option<&ProgressBar>)
    -> Vec<ParsedFilter> {
    let mut parsed_filters: Vec<ParsedFilter> = Vec::with_capacity(filters.len());
    for filter in filters {
        let aexp = if anchored && !filter.expression.starts_with('^') {
            format!("^{}", filter.expression)
        } else {
            filter.expression.clone()
        };
        let regex = match RegexBuilder::new(&aexp)
            .case_insensitive(ignore_case)
            .unicode(!no_unicode)
            .build()
        {
            Ok(regex) => regex,
            Err(e) => {
                panic!(
                    "INVARIANT ERROR: Previously valid Regex could not be parsed. Error: {e}, \
                        Assembled Regex: {aexp}, Source: {}, Line: {}",
                    filter.from,
                    filter.line.expect(&format!(
                        "Line may only be empty for user defined {operation} filters."
                    ))
                )
            }
        };
        parsed_filters.push(ParsedFilter {
            id: filter.id,
            expression: regex,
        });
        if let Some(bar) = cp {
            bar.inc(1);
        }
    }
    parsed_filters
}

/// Match a single record against the include and exclude sets, producing a `FilterResult`.
pub fn test_match(
    include: &[ParsedFilter],
    exclude: &[ParsedFilter],
    record: &StrippedRecord)
    -> FilterResult {

    let mut include_reason = 0i64;
    let mut exclude_reason = 0i64;

    // Match the original path bytes (raw identity), not the lossy text form:
    // byte-oriented filters (`--no-unicode`, `.` matches a byte) must be able to
    // reach non-UTF-8 path bytes that `to_string_lossy()` would have replaced.
    let haystack = record.fs_path().as_os_str().as_encoded_bytes();
    for filter in include {
        if filter.expression.is_match(haystack) {
            include_reason = filter.id;
            break;
        }
    }
    for filter in exclude {
        if filter.expression.is_match(haystack) {
            exclude_reason = filter.id;
            break;
        }
    }

    FilterResult {
        id: record.id,
        include_reason,
        exclude_reason,
    }
}

/// Insert a single pattern into the target filter table, verifying exactly one row
/// was added. Proxy for the database-agnostic insert.
pub struct FilterSink<'a> {
    pub add_include: Box<dyn Fn(&str, Option<u64>, &str) -> Result<u64> + 'a>,
    pub add_exclude: Box<dyn Fn(&str, Option<u64>, &str) -> Result<u64> + 'a>,
    /// Number of user include rules currently present (for the internal catch-all).
    pub count_includes: Box<dyn Fn() -> Result<u64> + 'a>,
}

/// Ingest the user's include/exclude patterns into the filtered pipeline state.
pub fn ingest_filters(
    include_patterns: &[String],
    include_from: &[PathBuf],
    exclude_patterns: &[String],
    exclude_from: &[PathBuf],
    ignore_case: bool,
    no_unicode: bool,
    fail_fast: bool,
    recorder: &mut Recorder,
    e_phase: ErrorPhase,
    sink: FilterSink<'_>)
    -> Result<()> {

    handle_pattern_source(
        include_patterns,
        include_from,
        "include",
        ignore_case,
        no_unicode,
        fail_fast,
        &sink.add_include,
        recorder,
        e_phase,
    )?;

    if (sink.count_includes)()? == 0 {
        let inc_fn = sink.add_include;
        let res = inc_fn("internal", None, ".*")?;
        assert_eq!(res, 1, "DB Failed, expected 1 row to get added, got {res}")
    }

    handle_pattern_source(
        exclude_patterns,
        exclude_from,
        "exclude",
        ignore_case,
        no_unicode,
        fail_fast,
        &sink.add_exclude,
        recorder,
        e_phase,
    )?;
    recorder.try_flush()?;
    Ok(())
}

/// Deal with one arm of inclusion / exclusion.
fn handle_pattern_source(
    pattern: &[String],
    files: &[PathBuf],
    operation: &str,
    ignore_case: bool,
    no_unicode: bool,
    fail_fast: bool,
    insert_fn: &dyn Fn(&str, Option<u64>, &str) -> Result<u64>,
    recorder: &mut Recorder,
    e_phase: ErrorPhase)
    -> Result<()> {

    // Scan single argument expressions.
    for (idx, query) in pattern.iter().enumerate() {
        handle_query(
            &format!("--{operation}"),
            query,
            operation,
            idx as u64,
            insert_fn,
            recorder,
            e_phase,
            ignore_case,
            no_unicode,
        )?;
    }

    // Scan files with content.
    for file in files.iter() {
        let pp = file.display();
        let file_content = match fs::read_to_string(file) {
            Ok(fc) => fc,
            Err(e) => {
                recorder.record_session(
                    e_phase,
                    FileStatError::Io {
                        path: file.clone(),
                        source: std::io::Error::new(e.kind(), e.to_string()),
                    },
                    ErrorFlags::default(),
                );
                if fail_fast {
                    return Err(Error::Config(format!(
                        "Could not read {operation} file: {pp} with error {e}")))
                } else {
                    tracing::error!("Could not read {operation} file: {pp} with error {e}");
                    continue;
                }
            }
        };
        for (idx, expression) in file_content.split("\n").enumerate() {
            if expression.is_empty() {
                continue;
            }
            let san_path = file.to_string_lossy();
            handle_query(
                &format!("--{operation}-from={san_path}"),
                expression,
                operation,
                idx as u64,
                insert_fn,
                recorder,
                e_phase,
                ignore_case,
                no_unicode,
            )?;
        }
    }
    Ok(())
}

/// Take care of inserting a single query into the database.
fn handle_query(
    source: &str,
    query: &str,
    operation: &str,
    line: u64,
    insert_fn: &dyn Fn(&str, Option<u64>, &str) -> Result<u64>,
    recorder: &mut Recorder,
    e_phase: ErrorPhase,
    ignore_case: bool,
    no_unicode: bool)
    -> Result<()> {

    match RegexBuilder::new(query)
        .case_insensitive(ignore_case)
        .unicode(!no_unicode)
        .build() {
        Ok(_regex) => {
            let res = insert_fn(source, Some(line), query)?;
            assert_eq!(res, 1, "DB Failed, expected 1 row to get added, got {res}");
        },
        Err(e) => {
            let error_msg = e.to_string();
            recorder.record_session(
                e_phase,
                FileStatError::General {
                    path: None,
                    message: format!(
                        "Failed to parse {operation} pattern from {source}, \
                                 line: {line}, expression: {query}, error: {error_msg}"
                    ),
                },
                ErrorFlags::default(),
            );
            tracing::error!("Failed to parse {operation} pattern from {source}, line: {line}, \
                expression: {query}, error: {error_msg}"
            );
        }
    }
    Ok(())
}

// TODO Tests

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PipelinePhase;
    use crate::db::flags::FileFlags;
    use crate::db::types::{FileId, FilePhase, FileType, StrippedRecord};
    use crate::test_support::TestWorld;
    use std::cell::RefCell;
    use std::ffi::OsString;
    use std::fs;
    use std::os::unix::ffi::OsStringExt;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::PathBuf;

    fn raw_record(name: &[u8]) -> StrippedRecord {
        let path = PathBuf::from(OsString::from_vec(name.to_vec()));
        StrippedRecord {
            id: FileId(1),
            abs_path: path.clone(),
            abs_path_raw: Some(path),
            ext: String::new(),
            size: 0,
            sha1: None,
            mtime: None, atime: None, ctime: None,
            ftype: FileType::File,
            device_id: None, inode_id: None,
            canonical_id: None,
            flags: FileFlags::from_i64(0),
            phase: FilePhase::Inventoried,
            new_name: None,
        }
    }

    fn filt(id: i64, pattern: &str, no_unicode: bool) -> ParsedFilter {
        ParsedFilter {
            id,
            expression: RegexBuilder::new(pattern)
                .unicode(!no_unicode)
                .build()
                .expect("filter compiles"),
        }
    }

    #[test]
    fn byte_filter_matches_raw_non_utf8_bytes() {
        // `bad-` + raw 0xFF 0xFE. Lossy conversion would replace both with U+FFFD,
        // so a byte-oriented pattern can only reach the original bytes.
        let record = raw_record(b"bad-\xFF\xFE");
        let include = vec![filt(-1, ".*", false)];
        let exclude = vec![filt(1, r"bad-\xFF\xFE", true)];
        let res = test_match(&include, &exclude, &record);
        assert_eq!(res.include_reason, -1);
        assert_eq!(res.exclude_reason, 1);

        // Counter-factual: the same pattern does NOT match the lossy text form.
        let lossy = record.abs_path.to_string_lossy();
        assert!(!exclude[0].expression.is_match(lossy.as_bytes()));
    }

    #[test]
    fn utf8_filter_matches_valid_utf8_path() {
        // Valid-UTF-8 paths are byte-identical to their text form, so matching is
        // unchanged from the lossy-string era.
        let record = raw_record("/some/café dir".as_bytes());
        let include = vec![filt(-1, ".*", false)];
        let exclude = vec![filt(1, "café", false)];
        let res = test_match(&include, &exclude, &record);
        assert_eq!(res.include_reason, -1);
        assert_eq!(res.exclude_reason, 1);
    }

    // ---- test_match matrix -------------------------------------------------

    #[test]
    fn test_match_base_cases_both_path_kinds() {
        let utf8 = raw_record("/tree/café".as_bytes());
        let raw = raw_record(b"/tree/bad-\xFF\xFE");
        for record in [&utf8, &raw] {
            let none: Vec<ParsedFilter> = Vec::new();
            let inc = vec![filt(-1, ".*", false)];
            let exc = vec![filt(1, ".*", false)];

            // (no include, no exclude) -> (0, 0)
            let r = test_match(&none, &none, record);
            assert_eq!((r.include_reason, r.exclude_reason), (0, 0));

            // (include, no exclude) -> (<0, 0)
            let r = test_match(&inc, &none, record);
            assert!(r.include_reason < 0 && r.exclude_reason == 0);

            // (no include, exclude) -> (0, >0)
            let r = test_match(&none, &exc, record);
            assert!(r.include_reason == 0 && r.exclude_reason > 0);

            // (include, exclude) -> (<0, >0)
            let r = test_match(&inc, &exc, record);
            assert!(r.include_reason < 0 && r.exclude_reason > 0);
        }
    }

    #[test]
    fn test_match_first_match_wins() {
        let record = raw_record(b"/x/a-b");
        let inc = vec![filt(-2, "a", false), filt(-1, ".*", false)];
        let exc = vec![filt(1, "b", false), filt(2, ".*", false)];
        let r = test_match(&inc, &exc, &record);
        assert_eq!(r.include_reason, -2, "first matching include wins");
        assert_eq!(r.exclude_reason, 1, "first matching exclude wins");
    }

    // ---- parse_filter ------------------------------------------------------

    #[test]
    fn parse_filter_anchored_once() {
        let fe = |expr: &str| FilterExpression {
            id: -1, from: "x".into(), line: Some(0), expression: expr.into(),
        };
        // `--anchored` prepends `^` exactly once...
        let a = parse_filter(&[fe("foo")], "include", true, false, false, None);
        assert_eq!(a[0].expression.as_str(), "^foo");
        // ...and never double-anchors an already-anchored expression.
        let b = parse_filter(&[fe("^foo")], "include", true, false, false, None);
        assert_eq!(b[0].expression.as_str(), "^foo");
        // Unanchored leaves the expression untouched.
        let c = parse_filter(&[fe("foo")], "include", false, false, false, None);
        assert_eq!(c[0].expression.as_str(), "foo");
    }

    #[test]
    fn parse_filter_case_and_unicode_flags() {
        let fe = FilterExpression {
            id: -1, from: "x".into(), line: Some(0), expression: "keep".into(),
        };
        let ic = parse_filter(&[fe.clone()], "include", false, true, false, None);
        assert!(ic[0].expression.is_match(b"KEEP"));
        let cs = parse_filter(&[fe], "include", false, false, false, None);
        assert!(!cs[0].expression.is_match(b"KEEP"));

        // Byte escape only compiles under `--no-unicode` (unicode mode rejects it).
        let nu_fe = FilterExpression {
            id: -1, from: "x".into(), line: Some(0), expression: r"\xFF".into(),
        };
        let nu = parse_filter(&[nu_fe.clone()], "include", false, false, true, None);
        assert!(nu[0].expression.is_match([0xFFu8].as_slice()));
    }

    #[test]
    fn parse_filter_panics_on_invalid_regex() {
        let fe = FilterExpression {
            id: -1, from: "x".into(), line: Some(0), expression: "[".into(),
        };
        let res = catch_unwind(AssertUnwindSafe(|| {
            let _ = parse_filter(&[fe], "include", false, false, false, None);
        }));
        assert!(res.is_err(), "invalid regex must panic (INVARIANT ERROR)");
    }

    // ---- handle_query ------------------------------------------------------

    type FilterTuple = (String, Option<u64>, String);

    fn query_sink<'a>(
        calls: &'a RefCell<Vec<FilterTuple>>) -> impl Fn(&str, Option<u64>, &str) -> crate::error::Result<u64> + 'a {
        move |from, line, query| {
            calls.borrow_mut().push((from.to_string(), line, query.to_string()));
            Ok(1)
        }
    }

    fn rec() -> Recorder<'static> {
        Recorder::speculative(false, None)
    }

    #[test]
    fn handle_query_inserts_valid_query() {
        let calls = RefCell::new(Vec::new());
        let insert = query_sink(&calls);
        handle_query("--include", "keep", "include", 0, &insert, &mut rec(),
            ErrorPhase::Pipeline(PipelinePhase::Filter), false, false).expect("ok");
        drop(insert);
        assert_eq!(calls.into_inner(), vec![("--include".into(), Some(0), "keep".into())]);
    }

    #[test]
    fn handle_query_ignores_invalid_query() {
        let calls = RefCell::new(Vec::new());
        let insert = query_sink(&calls);
        handle_query("--exclude", "[", "exclude", 3, &insert, &mut rec(),
            ErrorPhase::Pipeline(PipelinePhase::Filter), false, false).expect("ok");
        drop(insert);
        assert!(calls.into_inner().is_empty(), "invalid regex must not reach the sink");
    }

    #[test]
    fn handle_query_unicode_flags_reach_builder() {
        // `\p{Greek}` is a unicode-only construct: it compiles under unicode mode
        // (-> inserted) and is rejected under `--no-unicode` (-> recorded, not
        // inserted), proving the flag is plumbed into the builder.
        let ok = RefCell::new(Vec::new());
        handle_query("--include", r"\p{Greek}", "include", 0, &query_sink(&ok), &mut rec(),
            ErrorPhase::Pipeline(PipelinePhase::Filter), false, false).expect("ok");
        assert_eq!(ok.into_inner().len(), 1);

        let none = RefCell::new(Vec::new());
        handle_query("--include", r"\p{Greek}", "include", 0, &query_sink(&none), &mut rec(),
            ErrorPhase::Pipeline(PipelinePhase::Filter), false, true).expect("ok");
        assert!(none.into_inner().is_empty(), "no_unicode must reject the unicode class");
    }

    // ---- ingest_filters matrix ---------------------------------------------

    fn collecting_sink<'a>(
        includes: &'a RefCell<Vec<FilterTuple>>,
        excludes: &'a RefCell<Vec<FilterTuple>>,
    ) -> FilterSink<'a> {
        FilterSink {
            add_include: Box::new(|from, line, query| {
                includes.borrow_mut().push((from.to_string(), line, query.to_string()));
                Ok(1)
            }),
            add_exclude: Box::new(|from, line, query| {
                excludes.borrow_mut().push((from.to_string(), line, query.to_string()));
                Ok(1)
            }),
            count_includes: Box::new(|| Ok(includes.borrow().len() as u64)),
        }
    }

    #[test]
    fn ingest_filters_matrix() {
        // (cli patterns, expressions per file, from-files) sweep; the internal
        // catch-all is appended iff no user include landed.
        for &(cli, lines, files) in &[
            (0, 0, 0), (0, 0, 1), (0, 1, 0), (0, 1, 1), (0, 5, 2),
            (1, 0, 0), (1, 0, 1), (1, 1, 1), (5, 5, 3),
        ] {
            let world = TestWorld::new();
            let mut froms = Vec::new();
            for fi in 0..files {
                let p = world.path(&format!("inc{fi}.txt"));
                let body = (0..lines).map(|l| format!("p{fi}l{l}")).collect::<Vec<_>>().join("\n");
                fs::write(&p, body).expect("write");
                froms.push(p);
            }
            let cli_pats: Vec<String> = (0..cli).map(|i| format!("cli{i}")).collect();

            let includes = RefCell::new(Vec::new());
            let excludes = RefCell::new(Vec::new());
            ingest_filters(
                &cli_pats, &froms, &[], &[],
                false, false, false,
                &mut rec(), ErrorPhase::Pipeline(PipelinePhase::Filter),
                collecting_sink(&includes, &excludes),
            ).expect("ingest");

            let includes = includes.into_inner();
            let user = cli + files * lines;
            if user == 0 {
                assert_eq!(includes.len(), 1, "internal catch-all expected");
                assert_eq!(includes[0], ("internal".into(), None, ".*".into()));
            } else {
                assert_eq!(includes.len(), user, "no catch-all when user includes exist");
                // every from-file line carries its 0-based line index
                for f in 0..files {
                    for l in 0..lines {
                        let entry = &includes[cli + f * lines + l];
                        assert_eq!(entry.1, Some(l as u64));
                    }
                }
            }
            assert!(excludes.into_inner().is_empty());
        }
    }

    #[test]
    fn ingest_filters_joint_single_call() {
        let world = TestWorld::new();
        let inc = world.path("inc.txt");
        fs::write(&inc, "a\nb\n").expect("write");
        let empty = world.path("empty.txt");
        fs::write(&empty, "").expect("write");
        let broken = world.path("broken.txt");

        let includes = RefCell::new(Vec::new());
        let excludes = RefCell::new(Vec::new());
        ingest_filters(
            &["cli".into()], &[inc.clone(), empty, broken.clone()],
            &["exc".into()], &[inc.clone()],
            false, false, false,
            &mut rec(), ErrorPhase::Pipeline(PipelinePhase::Filter),
            collecting_sink(&includes, &excludes),
        ).expect("ingest");

        // includes: 1 cli + 2 from inc (empty/broken contribute nothing)
        let includes = includes.into_inner();
        assert_eq!(includes.len(), 3);
        assert_eq!(includes[0], ("--include".into(), Some(0), "cli".into()));
        // excludes: 1 cli + 2 from the same populated file
        let excludes = excludes.into_inner();
        assert_eq!(excludes.len(), 3);
        assert_eq!(excludes[0], ("--exclude".into(), Some(0), "exc".into()));
    }

    #[test]
    fn ingest_filters_broken_from_file_fail_fast() {
        let world = TestWorld::new();
        let broken = world.path("nope.txt");
        let includes = RefCell::new(Vec::new());
        let excludes = RefCell::new(Vec::new());
        let res = ingest_filters(
            &[], &[broken], &[], &[],
            false, false, true,
            &mut rec(), ErrorPhase::Pipeline(PipelinePhase::Filter),
            collecting_sink(&includes, &excludes),
        );
        assert!(matches!(res, Err(Error::Config(_))), "fail-fast must abort on a broken file");
    }

    // ---- step_filters / joint_filter_phase (DB-backed) ----------------------

    fn seed_hashed(world: &TestWorld, names: &[&str]) {
        for (i, name) in names.iter().enumerate() {
            let id = world.add_file(name, &[i as u8]);
            world.db.mark_file_phase(id, FilePhase::Hashed).expect("hashed");
        }
    }

    fn reason_pairs(world: &TestWorld) -> Vec<(i64, i64)> {
        world.db.with_transaction(|conn| {
            let mut stmt = conn.prepare(
                "SELECT include_reason_archive, exclude_reason_archive FROM files ORDER BY id",
            ).expect("stmt");
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            }).expect("query").collect::<rusqlite::Result<Vec<_>>>().expect("collect");
            Ok(rows)
        }).expect("tx")
    }

    /// Drop the TestWorld-seeded internal `.*` include so the table matches
    /// production when user filters are present (production only installs the
    /// catch-all when there are no user includes at all).
    fn clear_internal_include(world: &TestWorld) {
        world.db.with_transaction(|conn| {
            conn.execute("DELETE FROM filter_reason_archive WHERE id < 0", [])
                .expect("clear internal");
            Ok(())
        }).expect("tx");
    }

    /// The archive-side closures `joint_filter_phase` is called with in production.
    fn archive_closures(
        world: &TestWorld,
    ) -> (impl Fn(Option<bool>) -> Result<u64>,
          impl Fn(bool) -> Result<Vec<FilterExpression>>,
          impl Fn() -> Result<u64>,
          impl Fn(&FileId, u64) -> Result<Vec<StrippedRecord>>,
          impl Fn(Vec<(FileId, i64, i64)>) -> Result<u64>) {
        (
            move |is_exclude| world.db.count_filters_archive(is_exclude),
            move |is_exclude| world.db.get_filters_archive(is_exclude),
            move || world.db.apply_no_filter_archive(),
            move |lid, batch_size| world.db.get_rows_to_filter_archive(lid, false, batch_size),
            move |results| world.db.apply_filter_result_archive(results.into_iter()),
        )
    }

    #[test]
    fn joint_filter_phase_early_out_no_filters() {
        let world = TestWorld::new();
        // TestWorld seeds the internal `.*` include: 1 include, 0 excludes ->
        // the apply_no_filter fast path, step_filters never runs.
        seed_hashed(&world, &["a", "b"]);
        let (c, g, a, r, w) = archive_closures(&world);
        joint_filter_phase(
            false, false, false, false, 1000, FilePhase::Hashed,
            &world.db, &world.progress, c, g, a, r, w,
        ).expect("filter");

        assert_eq!(reason_pairs(&world), vec![(-1, 0), (-1, 0)]);
        assert_eq!(world.db.count_files_in_phase(FilePhase::Filtered).expect("c"), 2);
        assert_eq!(world.db.count_files_in_phase(FilePhase::Hashed).expect("c"), 0);
    }

    #[test]
    fn joint_filter_phase_counts_via_db_outcome() {
        let world = TestWorld::new();
        clear_internal_include(&world);
        seed_hashed(&world, &["keep_a", "keep_skip_b", "skip_c", "other_d"]);
        world.db.add_include_pattern("--include", Some(0), "keep").expect("incl");
        world.db.add_exclude_pattern("--exclude", Some(0), "skip").expect("excl");

        let (c, g, a, r, w) = archive_closures(&world);
        joint_filter_phase(
            false, false, false, false, 1000, FilePhase::Hashed,
            &world.db, &world.progress, c, g, a, r, w,
        ).expect("filter");

        // DB outcome, per the agreed cells (no internal `.*` in play):
        //   keep_a:      include -1, exclude 0  -> included
        //   keep_skip_b: include -1, exclude 1  -> excluded-by-exclude
        //   skip_c:      include 0,  exclude 1  -> excluded-by-include
        //   other_d:     include 0,  exclude 0  -> excluded-by-include
        assert_eq!(reason_pairs(&world), vec![(-1, 0), (-1, 1), (0, 1), (0, 0)]);
        assert_eq!(world.db.count_files_in_phase(FilePhase::Hashed).expect("c"), 0);
        assert_eq!(world.db.count_files_in_phase(FilePhase::Filtered).expect("c"), 4);
    }

    // ---- assert battery (catch_unwind) -------------------------------------

    #[test]
    fn step_filters_assert_positive_include_reason() {
        // A include filter carrying a positive id must trip the `include_reason <= 0`
        // guard: the invariant is "includes negative, excludes positive".
        let world = TestWorld::new();
        let res = catch_unwind(AssertUnwindSafe(|| {
            let _ = step_filters(
                false, false, false, 1000, &FilePhase::Hashed,
                &world.db, &world.progress,
                |is_exclude| Ok(if is_exclude == Some(true) { 0 } else { 1 }),
                |_is_exclude| Ok(vec![FilterExpression {
                    id: 5, from: "x".into(), line: None, expression: ".*".into(),
                }]),
                |_lid, _bs| Ok(vec![raw_record(b"/x")]),
                |_res| Ok(0),
            );
        }));
        assert!(res.is_err(), "a positive include id must trip the include_reason<=0 assert");
    }

    #[test]
    fn step_filters_assert_leftover_rows() {
        // A no-op apply_results leaves the hashed row in phase; the end-of-pass
        // zero-leftover assert must fire.
        let world = TestWorld::new();
        seed_hashed(&world, &["x"]);
        let res = catch_unwind(AssertUnwindSafe(|| {
            let _ = step_filters(
                false, false, false, 1000, &FilePhase::Hashed,
                &world.db, &world.progress,
                |is_exclude| Ok(if is_exclude == Some(true) { 0 } else { 1 }),
                |is_exclude| world.db.get_filters_archive(is_exclude),
                |lid, batch_size| world.db.get_rows_to_filter_archive(lid, false, batch_size),
                |results| Ok(results.len() as u64),
            );
        }));
        assert!(res.is_err(), "leftover hashed rows must trip the zero-leftover assert");
    }
}
