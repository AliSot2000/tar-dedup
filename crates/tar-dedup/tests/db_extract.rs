mod common;

use tar_dedup::config::ExtractRuntimeState;
use tar_dedup::db::flags::FileFlag;
use tar_dedup::db::types::{FilePhase, FileRecord};
use tar_dedup::db::{Database, ExtractScanState};

#[test]
fn install_initial_manifest_copies_embedded_snapshot() {
    let (work_dir, _) = common::open_temp_db();
    let manifest_path = work_dir.path().join("manifest.sqlite");
    common::write_archived_snapshot(&manifest_path, &["a.txt", "b.txt"]);

    let db_path = work_dir.path().join("extract.sqlite");
    Database::install_initial_manifest(&manifest_path, &db_path).expect("install manifest");

    let db = Database::open(&db_path).expect("open work db");
    assert_eq!(db.count_entries().expect("count"), 2);
}

#[test]
fn normalize_installed_catalog_clears_file_extracted_and_archives() {
    let (_dir, db) = common::open_temp_db();
    let (canonical_id, _) = common::seed_canonical_and_duplicate(
        &db,
        "canonical.txt",
        "duplicate.txt",
        "member-id",
        FilePhase::Staged,
    );
    db.set_flag(canonical_id, FileFlag::FileExtracted, true)
        .expect("set extracted");

    db.normalize_installed_catalog().expect("normalize");

    let canonical = db
        .get_file_by_id::<FileRecord>(canonical_id)
        .expect("get")
        .expect("row");
    assert!(!canonical.flags.get(FileFlag::FileExtracted));
    assert_eq!(canonical.phase, FilePhase::Archived);
}

#[test]
fn mark_file_extracted_sets_canonical_only_not_phase() {
    let (_dir, db) = common::open_temp_db();
    let (canonical_id, duplicate_id) = common::seed_canonical_and_duplicate(
        &db,
        "canonical.txt",
        "duplicate.txt",
        "member-id",
        FilePhase::Archived,
    );

    db.mark_file_extracted(canonical_id)
        .expect("mark extracted");

    let canonical = db
        .get_file_by_id::<FileRecord>(canonical_id)
        .expect("get")
        .expect("row");
    let duplicate = db
        .get_file_by_id::<FileRecord>(duplicate_id)
        .expect("get")
        .expect("row");

    assert!(canonical.flags.get(FileFlag::FileExtracted));
    assert!(!duplicate.flags.get(FileFlag::FileExtracted));
    assert_eq!(canonical.phase, FilePhase::Archived);
    assert_eq!(duplicate.phase, FilePhase::Archived);
}

#[test]
fn apply_snapshot_promotes_extracted_canonical_and_duplicates() {
    let (dir, db) = common::open_temp_db();
    let (canonical_id, duplicate_id) = common::seed_canonical_and_duplicate(
        &db,
        "canonical.txt",
        "duplicate.txt",
        "member-id",
        FilePhase::Archived,
    );
    db.mark_file_extracted(canonical_id)
        .expect("mark extracted");

    let snapshot_path = dir.path().join("progress.sqlite");
    common::write_archived_snapshot(&snapshot_path, &["canonical.txt"]);

    let promoted = db
        .apply_snapshot_promote_unarchived(&snapshot_path)
        .expect("apply snapshot");
    assert_eq!(promoted, 1);

    let canonical = db
        .get_file_by_id::<FileRecord>(canonical_id)
        .expect("get")
        .expect("row");
    let duplicate = db
        .get_file_by_id::<FileRecord>(duplicate_id)
        .expect("get")
        .expect("row");

    assert_eq!(canonical.phase, FilePhase::Unarchived);
    assert_eq!(duplicate.phase, FilePhase::Unarchived);
    assert!(canonical.flags.get(FileFlag::FileExtracted));
    assert!(!duplicate.flags.get(FileFlag::FileExtracted));
    assert_eq!(
        db.count_files_in_phase(FilePhase::Unarchived)
            .expect("unarchived count"),
        2
    );
    assert_eq!(db.skip_rehash().expect("skip rehash"), 2);
    assert_eq!(
        db.list_files_to_restore::<FileRecord>()
            .expect("restore list")
            .len(),
        2
    );
}

#[test]
fn promote_extracted_to_unarchived_salvage() {
    let (_dir, db) = common::open_temp_db();
    let (canonical_id, _) = common::seed_canonical_and_duplicate(
        &db,
        "canonical.txt",
        "duplicate.txt",
        "member-id",
        FilePhase::Archived,
    );
    db.mark_file_extracted(canonical_id).expect("mark");

    let n = db.promote_extracted_to_unarchived().expect("promote");
    assert!(n >= 1);
    assert_eq!(
        db.count_files_in_phase(FilePhase::Unarchived).expect("count"),
        2
    );
}

#[test]
fn extract_scan_state_round_trips_through_meta() {
    let (_dir, db) = common::open_temp_db();
    db.init_extract_runtime_state().expect("init");

    let state = ExtractScanState {
        saw_manifest_db: true,
        saw_any_members: true,
        scan_complete: false,
        last_member_index: Some(42),
        from_footer: true,
        snapshots_ingested: 3,
    };
    db.save_extract_scan_state(&state).expect("save");
    let loaded = db.load_extract_scan_state().expect("load");
    assert_eq!(loaded, state);

    // Clearing the member index has to remove the row, not leave the old value behind.
    let cleared = ExtractScanState {
        last_member_index: None,
        ..state
    };
    db.save_extract_scan_state(&cleared).expect("save cleared");
    assert_eq!(
        db.load_extract_scan_state().expect("load").last_member_index,
        None
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

#[test]
fn count_missing_and_extracted_reports() {
    let (_dir, db) = common::open_temp_db();
    let (canonical_id, _) = common::seed_canonical_and_duplicate(
        &db,
        "canonical.txt",
        "duplicate.txt",
        "member-id",
        FilePhase::Archived,
    );
    db.set_flag(canonical_id, FileFlag::AppendedPath, true)
        .expect("appended");

    assert_eq!(db.count_missing_payloads().expect("missing"), 1);
    assert_eq!(db.count_extracted_canonical().expect("canonical"), 0);

    db.mark_file_extracted(canonical_id).expect("extracted");
    assert_eq!(db.count_missing_payloads().expect("missing"), 0);
    assert_eq!(db.count_extracted_canonical().expect("canonical"), 1);
    assert_eq!(db.count_extracted_paths().expect("paths"), 2);
}

#[test]
fn dump_meta_and_clear_archive_meta() {
    let (_dir, db) = common::open_temp_db();
    db.init_extract_runtime_state().expect("init");
    db.set_archive_bytes_in(99).expect("bytes in");
    db.set_archive_bytes_out(100).expect("bytes out");

    let dump = db.dump_meta().expect("dump");
    assert!(dump
        .known
        .iter()
        .any(|e| matches!(e, tar_dedup::db::MetaEntry::ExtractPhase(_))));
    assert!(dump
        .known
        .iter()
        .any(|e| matches!(e, tar_dedup::db::MetaEntry::TarWriterBytesIn(99))));

    db.clear_archive_meta().expect("clear");
    assert_eq!(db.get_archive_bytes_in().expect("in"), 0);
    assert_eq!(db.get_archive_bytes_out().expect("out"), None);
    assert!(db
        .load_extract_runtime_state()
        .expect("load")
        .is_some());
}
