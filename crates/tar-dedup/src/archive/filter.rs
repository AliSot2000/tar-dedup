use crate::config::Config;
use crate::db::Database;
use crate::db::types::{FileId, FilePhase, FilterExpression, StrippedRecord};
use crate::error::Result;
use crate::shutdown::Shutdown;
use regex::{Regex, RegexBuilder};
use std::fs;
use std::path::PathBuf;

/// Stub filter stage: advance hashed → filtered before dedup.
pub fn run(db: &Database, config: &Config, shutdown: &Shutdown) -> Result<()> {
    let db_files = db.count_entries()?;
    let include_count = db.count_filters(Some(false))?;
    let exclude_count = db.count_filters(Some(true))?;

    // Handle case when nothing is
    if include_count == 1 && exclude_count == 0 {
        let include_filter = &db.get_filters(false)?[0];
        if include_filter.is_internal() {
            debug_assert_eq!(
                include_filter.expression, ".*",
                "Unexpected Filter expression. Filtering perhaps not working correctly?");

            let updated = db.apply_no_filter()?;
            tracing::info!("No filters present. All {updated} files selected.");
            debug_assert_eq!(db_files, updated, "Updated rows and total rows don't match");
            return Ok(());
        }
    }
    // Perform actual process of filtering. In case this is a noticeable bottleneck, it is a
    // separate function so we can swap in a rayon pool or a crossbeam ... whatever is better.
    fast_filter(&db, &config, &shutdown)?;
    let (down, up) = db.fix_up_canonical_flag()?;
    assert_eq!(down, up, "Number of clusters with downgrades did not match numbers with upgrade");
    let prev_phase = match config.eager_filter {
        true => FilePhase::Inventoried,
        false => FilePhase::Hashed,
    };
    assert_eq!(0, db.count_files_in_phase(prev_phase)?, "Files left over after the filtering");
    Ok(())
}

/// Perform the filtering of files as fast as possible. Currently, with lazy map iterators to avoid
/// creating two memcopies.
fn fast_filter(db: &Database, config: &Config, shutdown: &Shutdown) -> Result<()> {
    let include_filters =
        parse_filter(&db.get_filters(false)?, "include", &config);
    let exclude_filters =
        parse_filter(&db.get_filters(true)?, "exclude", &config);

    const BATCH_SIZE: u64 = 100_000;
    let mut last_id = None;
    loop {
        shutdown.check_between_files()?;

        let batch: Vec<StrippedRecord> = db.get_rows_to_filter(
            last_id, config.eager_filter, BATCH_SIZE
        )?;
        if batch.is_empty() { break; }

        // PRECONDITION: batch not empty
        last_id = Some(batch
            .last()
            .expect("INVARIANT ERROR: Batch empty, should contain something")
            .id);
        let processed = batch
            .iter()
            .map(|rec| test_match(&include_filters, &exclude_filters, &rec));
        let updated = db.apply_filter_result(
            processed.map(|fr| (fr.id, fr.include_reason,  fr.exclude_reason))
        )?;
        assert_eq!(updated, batch.len() as u64,
                   "INVARIANT ERROR: Number of rows updated does not match rows queried. \
                   Rows vanished?");
    }
    let rem = db.count_files_in_phase(
        if config.eager_filter {FilePhase::Inventoried} else {FilePhase::Hashed}
    )?;
    assert_eq!(0, rem, "INVARIANT ERROR: {rem} files in previous phase. Zero expected.");
    Ok(())
}

/// Convert the FilterExpression structs to ParsedFilter struct.
/// Applying --ignore-case and --anchored
/// Strong invariants assumed, violation will lead to panics
fn parse_filter(filters: &Vec<FilterExpression>, operation: &str, config: &Config)
    -> Vec<ParsedFilter> {
    let mut parsed_filters: Vec<ParsedFilter> = Vec::with_capacity(filters.len());
    for filter in filters.iter() {
        let aexp = if config.anchored && !filter.expression.starts_with('^') {
            &format!("^{}", filter.expression)
        } else {
            &filter.expression
        };
        let regex = match RegexBuilder::new(&aexp)
            .case_insensitive(config.ignore_case)
            .unicode(false)  // TODO needs to be done with --force-utf8
            .build(){
            Ok(regex) => regex,
            Err(e) => {
                panic!("INVARIANT ERROR: Previously valid Regex could not be parsed. Error: {e}, \
                Assembled Regex: {aexp}, Source: {}, Line: {}", filter.from, filter.line.expect(
                    &format!("Line may only be empty for user defined {operation} filters.")
                ))
            }
        };
        parsed_filters.push(ParsedFilter{
            id: filter.id,
            expression: regex,
        });
    }
    parsed_filters
}

/// Do the match checking for the include and the exclude filters and produce a FilterResult
fn test_match(include: &Vec<ParsedFilter>, exclude: &Vec<ParsedFilter>, record: &StrippedRecord)
    -> FilterResult {
    let mut include_reason = 0i64;
    let mut exclude_reason = 0i64;

    for filter in include.iter() {
        if filter.expression.is_match(&record.abs_path.to_string_lossy()) {
            include_reason = filter.id;
            break
        }
    }

    for filter in exclude.iter() {
        if filter.expression.is_match(&record.abs_path.to_string_lossy()) {
            exclude_reason = filter.id;
            break
        }
    }

    FilterResult {
        id: record.id,
        include_reason,
        exclude_reason,
    }
}

struct ParsedFilter {
    id: i64,
    expression: Regex,
}

struct FilterResult {
    id: FileId,
    // INFO: Include is negative!!! we need i64
    include_reason: i64,
    exclude_reason: i64,
}

/// Parse the arguments and add them into the database.
pub fn ingest_filters(db: &Database, config: &Config) -> Result<()> {
    // Handle the include files
    handle_filter(
        &config.include_patterns, &config.include_from, "include",
        &|from, line, query| db.add_include_pattern(from, line, query))?;

    if db.count_filters(Some(false))? == 0 {
        let res = db.add_include_pattern("internal", None, ".*")?;
        assert_eq!(res, 1, "DB Failed, expected 1 row to get added, got {res}")
    }

    handle_filter(
        &config.exclude_patterns, &config.exclude_from, "exclude",
        &|from, line, query| db.add_exclude_pattern(from, line, query))?;
    Ok(())
}

/// deal with one arm of inclusion / exclusion
fn handle_filter(
    pattern: &Vec<String>,
    files: &Vec<PathBuf>,
    operation: &str,
    insert_fn: &dyn Fn(&str, Option<u64>, &str) -> Result<u64>) -> Result<()>{

    // Scan single argument expression
    for (idx, query) in pattern.iter().enumerate() {
        handle_query(&format!("--{operation}"), query, operation, idx as u64, insert_fn)?;
    }

    // Scan files with content.
    for file in files.iter() {
        let pp = file.display();
        let file_content = match fs::read_to_string(file) {
            Ok(fc) => fc,
            Err(e) => {
                tracing::error!("Could not read {operation} file: {pp} with error {e}");
                continue;
                // TODO: Raise error without fail-fast
            }
        };
        for (idx, expression) in file_content.split("\n").enumerate() {
            if expression.is_empty(){
                continue
            }
            let san_path = file.to_string_lossy();
            handle_query(&format!("--{operation}-from={san_path}"), expression, operation,
                         idx as u64, insert_fn)?;

        }
    }
    Ok(())
}

/// Take care of inserting a single query into the database.
fn handle_query(source: &str, query: &str, operation: &str, line: u64,
                insert_fn: &dyn Fn(&str, Option<u64>, &str) -> Result<u64>) -> Result<()> {
    if Regex::new(query).is_ok() {
        let res = insert_fn(source, Some(line), query)?;
        assert_eq!(res, 1, "DB Failed, expected 1 row to get added, got {res}");
    } else {
        // TODO: Raise error without fail-fast
        tracing::error!(
            "Failed to parse {operation} pattern from {source}, line: {line}, expression: {query}"
        );
    }
    Ok(())
}