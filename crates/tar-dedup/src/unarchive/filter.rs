//! Extract filter phase: pure DB pass applying the user's include/exclude rules
//! against `files.abs_path`, writing `include_reason_extract` / `exclude_reason_extract`.

use crate::common::filter::ingest_filters as internal_ingest_filter;
use crate::common::filter::{FilterSink, parse_filter, test_match};
use crate::config::ExtractConfig;
use crate::db::types::StrippedRecord;
use crate::db::{ErrorPhase, Recorder};
use crate::error::Result;
use crate::unarchive::ExtractRTArgs;
use std::cell::RefCell;
use crate::progress::BarKind;

const BATCH_SIZE: u64 = 100_000;
const ERROR_PHASE: ErrorPhase = ErrorPhase::Extract(crate::config::ExtractPipelinePhase::Filter);

#[derive(Clone, Debug, Default)]
pub struct ParseFilterBuffer {
    include_filters: Vec<(String, Option<u64>, String)>,
    exclude_filters: Vec<(String, Option<u64>, String)>,
}

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
pub fn run(rt: &ExtractRTArgs) -> Result<()> {
    let db = rt.db;
    // TODO this guard should not ever fire. Check scan stage.
    if db.count_entries()? == 0 {
        tracing::info!("No catalog present; extract filters not applied (best-effort extraction).");
        return Ok(());
    }

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
            rt.progress.inc_global(updated);
            tracing::info!("No extract filters present. All {updated} files selected.");
            debug_assert_eq!(db_files, updated, "Updated rows and total rows don't match");
            return Ok(());
        }
    }

    fast_filter(rt)
}

/// Batched regex match of every `files` row against the extract rules.
fn fast_filter(rt: &ExtractRTArgs) -> Result<()> {
    let config = rt.config;
    let db = rt.db;
    let shutdown = rt.shutdown;
    let add_flt = rt.progress.push_sub_bar("Loading Filters from the DB...",
                                           BarKind::Counter);
    let include_count = db.count_filters(Some(false))?;
    let exclude_count = db.count_filters(Some(true))?;
    add_flt.set_length(include_count + exclude_count);

    let include_filters = parse_filter(
        &db.get_filters_extract(false)?,
        "include",
        config.filter.anchored,
        config.filter.ignore_case,
        Some(&add_flt)
    );
    let exclude_filters = parse_filter(
        &db.get_filters_extract(true)?,
        "exclude",
        config.filter.anchored,
        config.filter.ignore_case,
        Some(&add_flt)
    );

    drop(add_flt);

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
        // TODO counts and progresssbar, check filtering
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
        rt.progress.inc_both(updated);
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