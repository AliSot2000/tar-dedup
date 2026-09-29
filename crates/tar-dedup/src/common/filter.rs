//! Shared filter application used by both the archive and extract pipelines.

use crate::db::flags::ErrorFlags;
use crate::db::types::{FileId, FilePhase, FilterExpression, StrippedRecord};
use crate::db::{Database, ErrorPhase, Recorder};
use crate::error::{FileStatError, Result};
use indicatif::ProgressBar;
use regex::{Regex, RegexBuilder};
use std::fs;
use std::path::PathBuf;
use crate::common::batched_stepped_loop;
use crate::progress::{BarKind, ProgressBarSet};

const REGEX_UTF_8: bool = true;

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

    let db_files = db.count_entries()?;
    let include_count = count_filters(Some(false))?;
    let exclude_count = count_filters(Some(true))?;

    // Handle case when nothing is
    if include_count == 1 && exclude_count == 0 {
        let include_filter = &get_filters(false)?[0];
        if include_filter.is_internal() {
            pg.set_phase_total(db_files);
            debug_assert_eq!(
                include_filter.expression, ".*",
                "Unexpected Filter expression. Filtering perhaps not working correctly?"
            );

            let updated = apply_no_filter()?;
            pg.inc_global(updated);
            tracing::info!("No filters present. All {updated} files selected.");
            debug_assert_eq!(db_files, updated, "Updated rows and total rows don't match");
            pg.inc_both(db_files);
            return Ok(());
        }
    };
    // Perform actual process of filtering. In case this is a noticeable bottleneck, it is a
    // separate function so we can swap in a rayon pool or a crossbeam ... whatever is better.
    step_filters(
        anchored, ignore_case, batch_size, &pv, db, pg, get_rows_to_filter, apply_results
    )?;

    if update_hardlink_detection {
        let (down, up) = db.fix_up_canonical_flag()?;
        assert_eq!(down, up, "Number of clusters with downgrades did not match numbers with upgrade");
    }
    assert_eq!(0, db.count_files_in_phase(pv)?, "Files left over after the filtering");
    Ok(())
}

pub fn step_filters(
    anchored: bool,
    ignore_case: bool,
    batch_size: u64,
    pv: &FilePhase,
    db: &Database,
    pg: &ProgressBarSet,
    // Closures to stand in for the db
    get_rows_to_filter: impl Fn(&FileId, u64) -> Result<Vec<StrippedRecord>>,
    apply_results: impl Fn(Vec<(FileId, i64, i64)>) -> Result<u64>)
    -> Result<()> {
    // Filtering Progress Setup
    let already_filtered = db.count_files_in_phase(*pv)?;
    let total = db.count_entries()?;
    pg.set_phase_total(total);
    pg.inc_both(already_filtered);

    // Filter parsing
    let add_flt = pg.push_sub_bar(
        "Loading Filters from the DB...", BarKind::Count
    );
    add_flt.set_length(db.count_filters_archive(Some(true))?
        + db.count_filters_archive(Some(false))?);

    let include_filters = parse_filter(
        &db.get_filters_archive(false)?,
        "include",
        anchored,
        ignore_case,
        Some(&add_flt));
    let exclude_filters = parse_filter(
        &db.get_filters_archive(true)?,
        "exclude",
        anchored,
        ignore_case,
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

    let rem = db.count_files_in_phase(*pv)?;
    tracing::info!("Filtered {total} entries, deemed: {included} included, {exclude_by_include} \
        excluded by include filter, {exclude_by_exclude} excluded by exclude filter.");
    assert_eq!(0, rem, "INVARIANT ERROR: {rem} files in previous phase. Zero expected.");
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
            .unicode(REGEX_UTF_8) // TODO needs to be done with --force-utf8
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
    record: &StrippedRecord,
) -> FilterResult {
    let mut include_reason = 0i64;
    let mut exclude_reason = 0i64;

    let path = record.abs_path.to_string_lossy();
    for filter in include {
        if filter.expression.is_match(&path) {
            include_reason = filter.id;
            break;
        }
    }
    for filter in exclude {
        if filter.expression.is_match(&path) {
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
    recorder: &mut Recorder,
    e_phase: ErrorPhase,
    sink: FilterSink<'_>,
) -> Result<()> {
    handle_pattern_source(
        include_patterns,
        include_from,
        "include",
        ignore_case,
        &sink.add_include,
        recorder,
        e_phase,
    )?;

    if (sink.count_includes)()? == 0 {
        let res = (sink.add_include)("internal", None, ".*")?;
        assert_eq!(res, 1, "DB Failed, expected 1 row to get added, got {res}")
    }

    handle_pattern_source(
        exclude_patterns,
        exclude_from,
        "exclude",
        ignore_case,
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
    insert_fn: &dyn Fn(&str, Option<u64>, &str) -> Result<u64>,
    recorder: &mut Recorder,
    e_phase: ErrorPhase,
) -> Result<()> {
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
                tracing::error!("Could not read {operation} file: {pp} with error {e}");
                continue;
                // TODO: Raise error without fail-fast
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
    ignore_case: bool)
    -> Result<()> {
    match RegexBuilder::new(query)
        .case_insensitive(ignore_case)
        .unicode(REGEX_UTF_8) // TODO needs to be done with --force-utf8
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
