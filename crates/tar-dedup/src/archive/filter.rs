use std::fs;
use std::path::PathBuf;
use regex::Regex;
use crate::db::Database;
use crate::error::Result;
use crate::config::Config;

/// Stub filter stage: advance hashed → filtered before dedup.
pub fn run(db: &Database) -> Result<()> {
    let promoted = db.promote_hashed_to_filtered()?;
    if promoted > 0 {
        tracing::info!(count = promoted, "promoted hashed → filtered");
    }
    Ok(())
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