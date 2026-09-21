use crate::archive::ArchiveRTArgs;
use crate::common::filter::{
    FilterSink, ingest_filters as ingest_filter_rules, parse_filter, test_match,
};
use crate::config::ArchiveConfig;
use crate::db::Database;
use crate::db::types::{FilePhase, StrippedRecord};
use crate::error::Result;
use crate::progress::BarKind;

const BATCH_SIZE: u64 = 100_000;

/// Stub filter stage: advance hashed → filtered before dedup.
pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    let config = rt.config;
    let db = rt.db;
    let db_files = db.count_entries()?;
    let include_count = db.count_filters(Some(false))?;
    let exclude_count = db.count_filters(Some(true))?;

    // Handle case when nothing is
    if include_count == 1 && exclude_count == 0 {
        let include_filter = &db.get_filters(false)?[0];
        if include_filter.is_internal() {
            rt.progress.set_phase_total(db_files);
            debug_assert_eq!(
                include_filter.expression, ".*",
                "Unexpected Filter expression. Filtering perhaps not working correctly?");

            let updated = db.apply_no_filter()?;
            rt.progress.inc_global(updated);
            tracing::info!("No filters present. All {updated} files selected.");
            debug_assert_eq!(db_files, updated, "Updated rows and total rows don't match");
            rt.progress.inc_both(db_files);
            return Ok(());
        }
    }
    // Perform actual process of filtering. In case this is a noticeable bottleneck, it is a
    // separate function so we can swap in a rayon pool or a crossbeam ... whatever is better.
    fast_filter(rt)?;
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
fn fast_filter(rt: &ArchiveRTArgs) -> Result<()> {
    let config = rt.config;
    let db = rt.db;
    let shutdown = rt.shutdown;

    // Filtering Progress Setup
    let already_filtered = db.count_files_in_phase(FilePhase::Filtered)?;
    let total = db.count_entries()?;
    rt.progress.set_phase_total(total);
    rt.progress.inc_both(already_filtered);

    // Filter parsing
    let add_flt = rt.progress.push_sub_bar("Loading Filters from the DB...", BarKind::Count);
    add_flt.set_length(db.count_filters(Some(true))? + db.count_filters(Some(false))?);

    let include_filters = parse_filter(
        &db.get_filters(false)?,
        "include",
        config.filter.anchored,
        config.filter.ignore_case,
        Some(&add_flt));
    let exclude_filters = parse_filter(
        &db.get_filters(true)?,
        "exclude",
        config.filter.anchored,
        config.filter.ignore_case,
        Some(&add_flt));

    drop(add_flt);

    // Perform filtering
    let mut included = 0u64;
    let mut exclude_by_include = 0u64;
    let mut exclude_by_exclude = 0u64;

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
                rt.progress.inc_both(1);
                filt_res
            });
        let updated = db.apply_filter_result(
            processed.map(|fr| (fr.id, fr.include_reason, fr.exclude_reason)),
        )?;
        assert_eq!(
            updated, batch.len() as u64,
            "INVARIANT ERROR: Number of rows updated does not match rows queried. \
                   Rows vanished?"
        );
        rt.progress.inc_both(updated);
    }
    let rem = db.count_files_in_phase(if config.filter.eager_filter {
        FilePhase::Inventoried
    } else {
        FilePhase::Hashed
    })?;
    tracing::info!("Filtered {total} entries, deemed: {included} included, {exclude_by_include} \
        excluded by include filter, {exclude_by_exclude} excluded by exclude filter.");
    assert_eq!(0, rem, "INVARIANT ERROR: {rem} files in previous phase. Zero expected.");
    // TODO sanity check, filters set, and not null
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
        config.filter.ignore_case,
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
