//! Extract filter phase: pure DB pass applying the user's include/exclude rules
//! against `files.abs_path`, writing `include_reason_extract` / `exclude_reason_extract`.

use crate::common::filter::{FilterSink, ingest_filters, parse_filter, test_match};
use crate::config::ExtractConfig;
use crate::db::types::StrippedRecord;
use crate::db::{Database, ErrorPhase, Recorder};
use crate::error::Result;
use crate::shutdown::Shutdown;

const BATCH_SIZE: u64 = 100_000;
const ERROR_PHASE: ErrorPhase = ErrorPhase::Extract(crate::config::ExtractPipelinePhase::Filter);

// INFO: When no manifest DB was found (truncated / non-conform archive), the extract
//  filter pass is a deliberate no-op: there is nothing to match against, and filters
//  stay in `ExtractConfig`. Verified via the row count.
pub fn run(db: &Database, config: &ExtractConfig, shutdown: &Shutdown) -> Result<()> {
    if db.count_entries()? == 0 {
        tracing::info!("No catalog present; extract filters not applied (best-effort extraction).");
        return Ok(());
    }

    // Idempotency on resume: drop previous rules and reasons, then re-ingest.
    db.clear_extract_filters()?;

    let mut recorder = Recorder::new(db, !config.process.no_errors);
    ingest_filters(
        &config.filter.include_patterns,
        &config.filter.include_from,
        &config.filter.exclude_patterns,
        &config.filter.exclude_from,
        &mut recorder,
        ERROR_PHASE,
        FilterSink {
            add_include: Box::new(|from, line, query| {
                db.add_include_pattern_extract(from, line, query)
            }),
            add_exclude: Box::new(|from, line, query| {
                db.add_exclude_pattern_extract(from, line, query)
            }),
            count_includes: Box::new(|| db.count_filters_extract(Some(false))),
        },
    )?;

    let db_files = db.count_entries()?;
    let include_count = db.count_filters_extract(Some(false))?;
    let exclude_count = db.count_filters_extract(Some(true))?;

    if include_count == 1 && exclude_count == 0 {
        let include_filter = &db.get_filters_extract(false)?[0];
        if include_filter.is_internal() {
            debug_assert_eq!(
                include_filter.expression, ".*",
                "Unexpected Filter expression. Filtering perhaps not working correctly?"
            );

            let updated = db.apply_no_filter_extract()?;
            tracing::info!("No extract filters present. All {updated} files selected.");
            debug_assert_eq!(db_files, updated, "Updated rows and total rows don't match");
            return Ok(());
        }
    }

    fast_filter(db, config, shutdown)
}

/// Batched regex match of every `files` row against the extract rules.
fn fast_filter(db: &Database, config: &ExtractConfig, shutdown: &Shutdown) -> Result<()> {
    let include_filters = parse_filter(
        &db.get_filters_extract(false)?,
        "include",
        config.filter.anchored,
        config.filter.ignore_case,
    );
    let exclude_filters = parse_filter(
        &db.get_filters_extract(true)?,
        "exclude",
        config.filter.anchored,
        config.filter.ignore_case,
    );

    let mut last_id = None;
    loop {
        shutdown.check_between_files()?;

        let batch: Vec<StrippedRecord> = db.get_rows_to_filter_extract(last_id, BATCH_SIZE)?;
        if batch.is_empty() {
            break;
        }

        last_id = Some(
            batch
                .last()
                .expect("INVARIANT ERROR: Batch empty, should contain something")
                .id,
        );
        let processed = batch
            .iter()
            .map(|rec| test_match(&include_filters, &exclude_filters, rec));
        let updated = db.apply_filter_result_extract(
            processed.map(|fr| (fr.id, fr.include_reason, fr.exclude_reason)),
        )?;
        assert_eq!(
            updated,
            batch.len() as u64,
            "INVARIANT ERROR: Number of rows updated does not match rows queried. \
                   Rows vanished?"
        );
    }
    Ok(())
}
