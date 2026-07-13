mod common;

use std::path::PathBuf;

use tar_dedup::config::ExtractRuntimeState;
use tar_dedup::db::types::FilePhase;
use tar_dedup::db::Database;

#[test]
fn install_initial_manifest_copies_embedded_snapshot() {
    let (work_dir, _) = common::open_temp_db();
    let manifest_path = work_dir.path().join("manifest.sqlite");
    common::write_archived_snapshot(&manifest_path, &["a.txt", "b.txt"]);

    let db_path = work_dir.path().join("extract.sqlite");
    Database::install_initial_manifest(&manifest_path, &db_path).expect("install manifest");

    let db = Database::open(&db_path).expect("open work db");
    assert_eq!(db.count_files().expect("count"), 2);
}

#[test]
fn promote_cached_tar_member_marks_canonical_and_duplicates_unarchived() {
    let (_dir, db) = common::open_temp_db();
    common::seed_canonical_and_duplicate(
        &db,
        "canonical.txt",
        "duplicate.txt",
        "member-id",
        FilePhase::Archived,
    );

    db.promote_cached_tar_member("member-id")
        .expect("promote cached member");

    for record in db.files_in_phase(FilePhase::Unarchived).expect("list") {
        assert!(!record.snapshot_archived);
    }
    assert_eq!(
        db.files_in_phase(FilePhase::Unarchived)
            .expect("list")
            .len(),
        2
    );
}

#[test]
fn apply_snapshot_archived_flags_confirms_catalog_without_blocking_restore() {
    let (dir, db) = common::open_temp_db();
    common::seed_canonical_and_duplicate(
        &db,
        "canonical.txt",
        "duplicate.txt",
        "member-id",
        FilePhase::Archived,
    );
    db.promote_cached_tar_member("member-id")
        .expect("promote cached member");

    let snapshot_path = dir.path().join("progress.sqlite");
    common::write_archived_snapshot(&snapshot_path, &["canonical.txt"]);

    let flagged = db
        .apply_snapshot_archived_flags(&snapshot_path)
        .expect("apply snapshot flags");
    assert_eq!(flagged, 1);

    let canonical = db
        .get_file(
            db.files_in_phase(FilePhase::Unarchived)
                .expect("list")
                .into_iter()
                .find(|f| f.rel_path == PathBuf::from("canonical.txt"))
                .expect("canonical row")
                .id,
        )
        .expect("get canonical")
        .expect("canonical exists");
    let duplicate = db
        .get_file(
            db.files_in_phase(FilePhase::Unarchived)
                .expect("list")
                .into_iter()
                .find(|f| f.rel_path == PathBuf::from("duplicate.txt"))
                .expect("duplicate row")
                .id,
        )
        .expect("get duplicate")
        .expect("duplicate exists");

    assert!(canonical.snapshot_archived);
    assert!(duplicate.snapshot_archived);
    assert_eq!(
        db.list_files_to_restore().expect("restore list").len(),
        2
    );
}

#[test]
fn extract_runtime_state_round_trips_through_meta() {
    let (_dir, db) = common::open_temp_db();
    db.init_extract_runtime_state().expect("init");

    let state = ExtractRuntimeState::new();
    db.save_extract_runtime_state(&state).expect("save");
    let loaded = db
        .load_extract_runtime_state()
        .expect("load")
        .expect("state present");
    assert_eq!(loaded, state);

    let after = db.record_snapshot_ingested().expect("record snapshot");
    assert_eq!(after, 1);
}
