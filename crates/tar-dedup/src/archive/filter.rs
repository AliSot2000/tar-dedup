use crate::archive::ArchiveRTArgs;
use crate::common::filter::{
    FilterSink, ingest_filters as ingest_filter_rules, joint_filter_phase,
};
use crate::config::ArchiveConfig;
use crate::db::Database;
use crate::db::types::FilePhase;
use crate::error::Result;

const BATCH_SIZE: u64 = 100_000;

/// Stub filter stage: advance hashed → filtered before dedup.
pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    let previous_phase = match rt.config.filter.eager_filter {
        true => FilePhase::Inventoried,
        false => FilePhase::Hashed,
    };
    joint_filter_phase(
        !rt.config.indexing.no_hardlink_detection,
        rt.config.filter.anchored,
        rt.config.filter.ignore_case,
        BATCH_SIZE,
        previous_phase,
        &rt.db,
        rt.progress,
        |is_exclude| rt.db.count_filters_archive(is_exclude),
        |is_exclude| rt.db.get_filters_archive(is_exclude),
        || rt.db.apply_no_filter_archive(),
        |lid, batch_size| rt.db.get_rows_to_filter_archive(
            lid, rt.config.filter.eager_filter, batch_size
        ),
        |results| rt.db.apply_filter_result_archive(results.into_iter())
    )
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
            count_includes: Box::new(|| db.count_filters_archive(Some(false))),
        },
    )?;
    Ok(())
}
