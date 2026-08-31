//! Shared helpers for integration tests (`tests/*.rs`).

use std::path::{Path, PathBuf};

use tar_dedup::cli::ConflictPolicy;
use tar_dedup::common::start::StartPolicy;
use tar_dedup::config::{
    CleanupSettings, CompressionFormat, ExtractAttributeOptions, ExtractConfig, PathLayout,
    PlacementOptions, ProcessOptions, ScanOptions,
};
use tar_dedup::db::flags::{SourceFlag, SourceFlags};
use tar_dedup::db::types::{FileId, FilePhase, FileType, NewFileRecord, StrippedRecord};
use tar_dedup::db::Database;
use tar_dedup::shutdown::Shutdown;
use tar_dedup::unarchive::populate_out_tree as populate_out_tree_impl;

pub fn open_temp_db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Database::open(&dir.path().join("snapshot.sqlite")).expect("open db");
    (dir, db)
}

pub fn insert_file(db: &Database, abs_path: &str, size: u64) -> FileId {
    use tar_dedup::common::files::original_extension;
    let path = PathBuf::from(abs_path);
    db.insert_file(&NewFileRecord {
        ext: original_extension(&path),
        abs_path: path,
        size,
        mtime: None,
        atime: None,
        ctime: None,
        uid: None,
        gid: None,
        mode: None,
        ftype: None,
        xattrs: None,
        posix_acl: None,
        selinux_ctx: None,
        link_dst: None,
        inode_id: None,
        device_id: None,
    })
    .expect("insert file");

    db.files_in_phase::<StrippedRecord>(FilePhase::Inventoried)
        .expect("list inventoried")
        .into_iter()
        .find(|f| f.abs_path == Path::new(abs_path))
        .expect("inserted file")
        .id
}

/// Canonical row plus a duplicate that shares its tar member.
pub fn seed_canonical_and_duplicate(
    db: &Database,
    canonical_rel: &str,
    duplicate_rel: &str,
    _tar_path: &str,
    phase: FilePhase,
) -> (FileId, FileId) {
    let canonical_id = insert_file(db, canonical_rel, 10);
    db.mark_self_canonical(canonical_id).expect("self canonical");

    let duplicate_id = insert_file(db, duplicate_rel, 10);
    db.set_canonical(duplicate_id, canonical_id)
        .expect("set canonical");

    db.mark_file_phase(canonical_id, phase).expect("canonical phase");
    db.mark_file_phase(duplicate_id, phase).expect("duplicate phase");

    (canonical_id, duplicate_id)
}

pub fn insert_materialized(
    db: &Database,
    abs_path: &str,
    ftype: FileType,
    size: u64,
) -> FileId {
    use tar_dedup::common::files::original_extension;
    let path = PathBuf::from(abs_path);
    db.insert_file(&NewFileRecord {
        ext: original_extension(&path),
        abs_path: path.clone(),
        size,
        mtime: None,
        atime: None,
        ctime: None,
        uid: None,
        gid: None,
        mode: None,
        ftype: Some(ftype),
        xattrs: None,
        posix_acl: None,
        selinux_ctx: None,
        link_dst: None,
        inode_id: None,
        device_id: None,
    })
    .expect("insert file");
    db.apply_no_filter().expect("include all");
    db.file_id_by_abs_path(&path)
        .expect("lookup")
        .expect("inserted file")
}

pub fn seed_source_dir(
    db: &Database,
    abs_path: &str,
    original_path: &str,
) -> i64 {
    db.add_get_source(
        Path::new(abs_path),
        "--input-dir",
        Some(0),
        Some(Path::new(original_path)),
        SourceFlags::default().with(SourceFlag::IsDirectory, true),
    )
    .expect("source dir")
}

pub fn place_config(extraction_root: PathBuf, absolute_names: bool) -> ExtractConfig {
    ExtractConfig {
        force: true,
        paths: PathLayout {
            archive_path: PathBuf::new(),
            directory: extraction_root,
            work_dir: PathBuf::new(),
        },
        decompression: CompressionFormat::None,
        placement: PlacementOptions {
            absolute_names,
            one_top_level: None,
            keep_dir_symlink: false,
            unlink_first: false,
            no_create_dir: true,
            conflict_policy: ConflictPolicy::Replace,
            silent_conflicts: false,
            remove_and_replace: false,
            link_tree: false,
            use_hard_links: false,
            absolute_links: false,
            hardlink_reestablish: true,
            clean_target: false,
            link_source: None,
            no_reflink: false,
        },
        attributes: ExtractAttributeOptions {
            restore_owner: false,
            no_overwrite_dir: false,
            force_overwrite_dir: false,
        },
        scan: ScanOptions {
            force_scan: false,
            rehash: true,
            clear_archive_meta: false,
        },
        process: ProcessOptions {
            start_policy: StartPolicy::Create,
            jobs: 1,
            fail_fast: false,
            no_errors: false,
            cleanup: CleanupSettings::from_flags(false, false),
            exit_after_stage: None,
        },
    }
}

pub fn populate_out_tree(db: &Database, config: &ExtractConfig) {
    populate_out_tree_impl(db, config, &Shutdown::detached()).expect("populate out_tree");
}

/// Write a standalone snapshot DB listing the given rel_paths as `archived`.
pub fn write_archived_snapshot(path: &Path, rel_paths: &[&str]) -> Database {
    if path.is_file() {
        std::fs::remove_file(path).expect("remove snapshot");
    }
    let db = Database::open(path).expect("open snapshot db");
    for rel_path in rel_paths {
        let id = insert_file(&db, rel_path, 1);
        db.mark_self_canonical(id).expect("self canonical");
        db.mark_file_phase(id, FilePhase::Archived)
            .expect("archived phase");
    }
    db
}
