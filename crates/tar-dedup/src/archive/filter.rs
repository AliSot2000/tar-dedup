use crate::common::filter::{
    FilterSink, ingest_filters as ingest_filter_rules, parse_filter, test_match,
};
use crate::config::ArchiveConfig;
use crate::db::Database;
use crate::db::types::{FilePhase, StrippedRecord};
use crate::error::Result;
use crate::shutdown::Shutdown;

/// Stub filter stage: advance hashed → filtered before dedup.
pub fn run(db: &Database, config: &ArchiveConfig, shutdown: &Shutdown) -> Result<()> {
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
    if !config.indexing.no_hardlink_detection {
        let (down, up) = db.fix_up_canonical_flag()?;
        assert_eq!(down, up, "Number of clusters with downgrades did not match numbers with upgrade");
    }
    let prev_phase = match config.filter.eager_filter {
        true => FilePhase::Inventoried,
        false => FilePhase::Hashed,
    };
    assert_eq!(0, db.count_files_in_phase(prev_phase)?, "Files left over after the filtering");
    Ok(())
}

/// Perform the filtering of files as fast as possible. Currently, with lazy map iterators to avoid
/// creating two memcopies.
fn fast_filter(db: &Database, config: &ArchiveConfig, shutdown: &Shutdown) -> Result<()> {
    let include_filters = parse_filter(
        &db.get_filters(false)?, "include", config.filter.anchored, config.filter.ignore_case);
    let exclude_filters = parse_filter(
        &db.get_filters(true)?, "exclude", config.filter.anchored, config.filter.ignore_case);

    const BATCH_SIZE: u64 = 100_000;
    let mut last_id = None;
    loop {
        shutdown.check_between_files()?;

        let batch: Vec<StrippedRecord> = db.get_rows_to_filter(
            last_id, config.filter.eager_filter, BATCH_SIZE
        )?;
        if batch.is_empty() { break; }

        // PRECONDITION: batch not empty
        last_id = Some(
            batch
                .last()
                .expect("INVARIANT ERROR: Batch empty, should contain something")
                .id);
        let processed = batch
            .iter()
            .map(|rec| test_match(&include_filters, &exclude_filters, rec));
        let updated = db.apply_filter_result(
            processed.map(|fr| (fr.id, fr.include_reason, fr.exclude_reason)),
        )?;
        assert_eq!(
            updated, batch.len() as u64,
            "INVARIANT ERROR: Number of rows updated does not match rows queried. \
                   Rows vanished?"
        );
    }
    let rem = db.count_files_in_phase(if config.filter.eager_filter {
        FilePhase::Inventoried
    } else {
        FilePhase::Hashed
    })?;
    assert_eq!(0, rem, "INVARIANT ERROR: {rem} files in previous phase. Zero expected.");
    Ok(())
}

/// Parse the arguments and add them into the database.
pub fn ingest_filters(db: &Database, config: &ArchiveConfig) -> Result<()> {
    let mut recorder = crate::db::Recorder::new(db, !config.process.no_errors);
    let phase = crate::db::ErrorPhase::Pipeline(crate::config::PipelinePhase::Filter);
    ingest_filter_rules(
        &config.filter.include_patterns,
        &config.filter.include_from,
        &config.filter.exclude_patterns,
        &config.filter.exclude_from,
        &mut recorder,
        phase,
        FilterSink {
            add_include: Box::new(|from, line, query|
                db.add_include_pattern(from, line, query)),
            add_exclude: Box::new(|from, line, query|
                db.add_exclude_pattern(from, line, query)),
            count_includes: Box::new(|| db.count_filters(Some(false))),
        },
    )?;
    Ok(())
}
