use crate::archive::ArchiveRTArgs;
use crate::common::filter::{
    FilterSink, ingest_filters as ingest_filter_rules, joint_filter_phase,
};
use crate::config::ArchiveConfig;
use crate::db::Database;
use crate::db::types::FilePhase;
use crate::error::Result;

/// Stub filter stage: advance hashed → filtered before dedup.
pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    let previous_phase = match rt.config.filter.eager_filter {
        true => FilePhase::Inventoried,
        false => FilePhase::Hashed,
    };
    let eligible = rt.db.count_files_in_phase(previous_phase)?;
    tracing::info!(
        "starting filter: total {}, processing {}, pending {}",
        rt.db.count_entries()?,
        eligible,
        eligible,
    );
    joint_filter_phase(
        !rt.config.indexing.no_hardlink_detection,
        rt.config.filter.anchored,
        rt.config.filter.ignore_case,
        rt.config.filter.no_unicode,
        rt.config.process.batch_size,
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
    let mut recorder = crate::db::Recorder::new(db, !config.process.no_errors, Some(config.process.auto_flush_limit));
    let phase = crate::db::ErrorPhase::Pipeline(crate::config::PipelinePhase::Filter);
    ingest_filter_rules(
        &config.filter.include_patterns,
        &config.filter.include_from,
        &config.filter.exclude_patterns,
        &config.filter.exclude_from,
        config.filter.ignore_case,
        config.filter.no_unicode,
        config.process.fail_fast,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestWorld;

    fn seed_hashed(world: &TestWorld, names: &[&str]) {
        for (i, name) in names.iter().enumerate() {
            let id = world.add_file(name, &[i as u8]);
            world.db.mark_file_phase(id, FilePhase::Hashed).expect("hashed");
        }
    }

    fn reason_pairs(world: &TestWorld) -> Vec<(i64, i64)> {
        world.db.with_transaction(|conn| {
            let mut stmt = conn.prepare(
                "SELECT include_reason_archive, exclude_reason_archive FROM files ORDER BY id",
            ).expect("stmt");
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            }).expect("query").collect::<rusqlite::Result<Vec<_>>>().expect("collect");
            Ok(rows)
        }).expect("tx")
    }

    /// Drop the TestWorld-seeded internal `.*` include so user-filter scenarios
    /// match production (the catch-all is only installed when there are no user
    /// includes).
    fn clear_internal_include(world: &TestWorld) {
        world.db.with_transaction(|conn| {
            conn.execute("DELETE FROM filter_reason_archive WHERE id < 0", [])
                .expect("clear internal");
            Ok(())
        }).expect("tx");
    }

    #[test]
    fn ingest_filters_populates_archive_rules() {
        let mut world = TestWorld::new();
        clear_internal_include(&world);
        world.config.filter.include_patterns = vec!["keep".into()];
        world.config.filter.exclude_patterns = vec!["skip".into()];
        ingest_filters(&world.db, &world.config).expect("ingest");

        let inc = world.db.get_filters_archive(false).expect("get");
        let exc = world.db.get_filters_archive(true).expect("get");
        // User includes exist -> no internal catch-all; `keep` takes id -1.
        assert_eq!(inc.len(), 1);
        assert_eq!(inc[0].expression, "keep");
        assert_eq!(exc.len(), 1);
        assert_eq!(exc[0].expression, "skip");
    }

    #[test]
    fn run_marks_filtered_and_reasons() {
        let mut world = TestWorld::new();
        clear_internal_include(&world);
        world.config.filter.include_patterns = vec!["keep".into()];
        world.config.filter.exclude_patterns = vec!["skip".into()];
        ingest_filters(&world.db, &world.config).expect("ingest");
        seed_hashed(&world, &["keep_a", "keep_skip_b", "skip_c", "other_d"]);

        run(&world.rt()).expect("run");

        assert_eq!(reason_pairs(&world), vec![(-1, 0), (-1, 1), (0, 1), (0, 0)]);
        assert_eq!(world.db.count_files_in_phase(FilePhase::Filtered).expect("c"), 4);
        assert_eq!(world.db.count_files_in_phase(FilePhase::Hashed).expect("c"), 0);
    }

    #[test]
    fn run_no_filters_takes_fast_path() {
        let world = TestWorld::new();
        seed_hashed(&world, &["a", "b"]);

        run(&world.rt()).expect("run");

        // The internal catch-all is the only include -> apply_no_filter fast path.
        assert_eq!(reason_pairs(&world), vec![(-1, 0), (-1, 0)]);
        assert_eq!(world.db.count_files_in_phase(FilePhase::Filtered).expect("c"), 2);
    }
}
