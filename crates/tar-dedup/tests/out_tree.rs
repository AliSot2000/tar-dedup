mod common;

use tar_dedup::db::types::FileType;

/// What: absolute-mode populate inserts catalog dirs/files and ancestor dirs.
/// Why: out_tree must mirror the full fixed output tree before place runs.
/// Particularities: ancestor-only rows have NULL file_id; catalog rows keep file_id.
#[test]
fn populate_out_tree_absolute_dirs_files_and_ancestor_null_file_id() {
    let (dir, db) = common::open_temp_db();
    let extract_root = dir.path().join("out");
    std::fs::create_dir_all(&extract_root).expect("extract root");

    let dir_id = common::insert_materialized(
        &db,
        "/project/src",
        FileType::Directory,
        0,
    );
    let file_id = common::insert_materialized(
        &db,
        "/project/src/main.rs",
        FileType::File,
        42,
    );

    let config = common::place_config(extract_root.clone(), true);
    common::populate_out_tree(&db, &config);

    assert!(db.out_tree_is_built().expect("meta"));
    assert_eq!(db.count_out_tree_rows().expect("count"), 4);
    assert_eq!(db.count_ref_out_rows().expect("ref_out"), 0);

    let file_row = db
        .list_out_tree_files(None, 100, None)
        .expect("files")
        .into_iter()
        .find(|r| r.file_id == Some(file_id))
        .expect("file row");
    assert_eq!(
        file_row.abs_path,
        extract_root.join("project/src/main.rs")
    );

    let dir_rows = db
        .list_out_tree_dirs(None, 100, None)
        .expect("dirs");
    assert_eq!(dir_rows.len(), 1);
    assert_eq!(dir_rows[0].file_id, Some(dir_id));
    assert_eq!(dir_rows[0].abs_path, extract_root.join("project/src"));

    let all = db.list_out_tree_batch(None, 100, None).expect("all");
    let ancestor_only: Vec<_> = all.iter().filter(|r| r.file_id.is_none()).collect();
    assert_eq!(ancestor_only.len(), 2);
    assert!(ancestor_only
        .iter()
        .any(|r| r.abs_path == extract_root.join("project")));
    assert!(ancestor_only
        .iter()
        .any(|r| r.abs_path == extract_root));
}

/// What: relative mode links each out_tree row to its source via ref_out.
/// Why: one catalog file_id may map to multiple output paths across sources.
/// Particularities: ref_out is empty in absolute mode.
#[test]
fn populate_out_tree_relative_ref_out_and_multi_file_id() {
    let (dir, db) = common::open_temp_db();
    let extract_root = dir.path().join("out");
    std::fs::create_dir_all(&extract_root).expect("extract root");

    let source_a = db.add_get_source(
        std::path::Path::new("/data/a"),
        "--input-dir",
        Some(0),
        Some(std::path::Path::new("a")),
        tar_dedup::db::flags::SourceFlags::default()
            .with(tar_dedup::db::flags::SourceFlag::IsDirectory, true),
    ).expect("source a");
    let source_b = db.add_get_source(
        std::path::Path::new("/data/a"),
        "--input-dir",
        Some(1),
        Some(std::path::Path::new("b")),
        tar_dedup::db::flags::SourceFlags::default()
            .with(tar_dedup::db::flags::SourceFlag::IsDirectory, true),
    ).expect("source b");

    let shared = common::insert_materialized(&db, "/data/a/shared.txt", FileType::File, 1);
    db.add_ref(source_a, shared).expect("ref a");
    db.add_ref(source_b, shared).expect("ref b");

    let config = common::place_config(extract_root.clone(), false);
    common::populate_out_tree(&db, &config);

    assert!(db.out_tree_is_built().expect("meta"));
    assert!(db.count_ref_out_rows().expect("ref_out") >= 2);

    let paths = db.out_paths_for_file_id(shared).expect("paths");
    assert_eq!(paths.len(), 2);
    assert!(paths.contains(&extract_root.join("a/shared.txt")));
    assert!(paths.contains(&extract_root.join("b/shared.txt")));

    let for_a = db
        .list_out_tree_files(None, 100, Some(source_a))
        .expect("source a files");
    assert!(for_a.iter().any(|r| r.file_id == Some(shared)));
}

/// What: dir vs non-dir filtering uses JOIN files, not out_tree alone.
/// Why: node kind lives on the catalog row; out_tree stores target paths only.
/// Particularities: ancestor NULL-file_id rows are excluded from dir/file lists.
#[test]
fn list_out_tree_dirs_joins_files_ftype() {
    let (dir, db) = common::open_temp_db();
    let extract_root = dir.path().join("out");
    std::fs::create_dir_all(&extract_root).expect("extract root");

    common::insert_materialized(&db, "/x/y", FileType::Directory, 0);
    common::insert_materialized(&db, "/x/y/z.txt", FileType::File, 3);

    let config = common::place_config(extract_root, true);
    common::populate_out_tree(&db, &config);

    assert_eq!(
        db.list_out_tree_dirs(None, 100, None)
            .expect("dirs")
            .len(),
        1
    );
    assert_eq!(
        db.list_out_tree_files(None, 100, None)
            .expect("files")
            .len(),
        1
    );
}

/// What: populate_out_tree is skipped when meta says out_tree_built.
/// Why: resume must not rebuild or duplicate ref_out rows.
/// Particularities: second call is a no-op; row counts unchanged.
#[test]
fn populate_out_tree_resume_is_idempotent() {
    let (dir, db) = common::open_temp_db();
    let extract_root = dir.path().join("out");
    std::fs::create_dir_all(&extract_root).expect("extract root");

    common::insert_materialized(&db, "/only.txt", FileType::File, 1);
    let config = common::place_config(extract_root, true);

    common::populate_out_tree(&db, &config);
    let first_count = db.count_out_tree_rows().expect("count");

    common::populate_out_tree(&db, &config);
    assert_eq!(db.count_out_tree_rows().expect("count again"), first_count);
    assert!(db.out_tree_is_built().expect("meta"));
}

/// What: insert_ref no longer accepts or stores per-membership flags.
/// Why: ref.flags was removed; extract state belongs on out_tree/files.
/// Particularities: add_ref succeeds with the two-column ref schema.
#[test]
fn insert_ref_without_flags() {
    let (_dir, db) = common::open_temp_db();
    let source = common::seed_source_dir(&db, "/src", "/src");
    let file_id = common::insert_materialized(&db, "/src/a.txt", FileType::File, 1);

    assert!(db.add_ref(source, file_id).expect("insert ref"));
    assert!(!db.add_ref(source, file_id).expect("duplicate ignored"));
}
