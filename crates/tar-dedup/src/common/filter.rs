//! Shared filter application used by both the archive and extract pipelines.

use crate::db::flags::ErrorFlags;
use crate::db::types::{FileId, FilterExpression, StrippedRecord};
use crate::db::{ErrorPhase, Recorder};
use crate::error::{FileStatError, Result};
use indicatif::ProgressBar;
use regex::{Regex, RegexBuilder};
use std::fs;
use std::path::PathBuf;

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

/// Convert filter expressions to compiled regexes, applying `--anchored` and
/// `--ignore-case`.
/// Strong invariants assumed, violation will lead to panics.
pub fn parse_filter(
    filters: &[FilterExpression],
    operation: &str,
    anchored: bool,
    ignore_case: bool,
    cp: Option<&ProgressBar>
) -> Vec<ParsedFilter> {
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
