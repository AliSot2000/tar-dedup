//! Extract filter phase: pure DB pass applying the user's include/exclude rules
//! against `files.abs_path`, writing `include_reason_extract` / `exclude_reason_extract`.

use crate::common::filter::ingest_filters as internal_ingest_filter;
use crate::common::filter::{FilterSink, joint_filter_phase};
use crate::config::ExtractConfig;
use crate::db::types::FilePhase::Unarchived;
use crate::db::{ErrorPhase, Recorder};
use crate::error::Result;
use crate::unarchive::ExtractRTArgs;
use std::cell::RefCell;

const BATCH_SIZE: u64 = 100_000;
const ERROR_PHASE: ErrorPhase = ErrorPhase::Extract(crate::config::ExtractPipelinePhase::Filter);

#[derive(Clone, Debug, Default)]
pub struct ParseFilterBuffer {
    include_filters: Vec<(String, Option<u64>, String)>,
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
pub fn run(rt: &ExtractRTArgs) -> Result<()> {
    joint_filter_phase(
        false,
        rt.config.filter.anchored,
        rt.config.filter.ignore_case,
        BATCH_SIZE,
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
    )
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