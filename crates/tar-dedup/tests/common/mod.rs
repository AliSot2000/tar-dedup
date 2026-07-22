//! Shared helpers for integration tests (`tests/*.rs`).

use std::path::{Path, PathBuf};

use tar_dedup::db::types::{FileId, FilePhase, NewFileRecord, StrippedRecord};
use tar_dedup::db::Database;

pub fn open_temp_db() -> (tempfile::TempDir, Database) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Database::open(&dir.path().join("snapshot.sqlite")).expect("open db");
    (dir, db)
}

pub fn insert_file(db: &Database, rel_path: &str, size: u64) -> FileId {
    db.insert_file(&NewFileRecord {
        rel_path: PathBuf::from(rel_path),
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
    })
    .expect("insert file");

    db.files_in_phase::<StrippedRecord>(FilePhase::Inventoried)
        .expect("list inventoried")
        .into_iter()
        .find(|f| f.rel_path == Path::new(rel_path))
        .expect("inserted file")
        .id
}

/// Canonical row plus a duplicate that shares its tar member.
pub fn seed_canonical_and_duplicate(
    db: &Database,
    canonical_rel: &str,
    duplicate_rel: &str,
    tar_path: &str,
    phase: FilePhase,
) -> (FileId, FileId) {
    let canonical_id = insert_file(db, canonical_rel, 10);
    db.mark_self_canonical(canonical_id).expect("self canonical");
    db.set_tar_path(canonical_id, tar_path).expect("tar path");

    let duplicate_id = insert_file(db, duplicate_rel, 10);
    db.set_canonical(duplicate_id, canonical_id)
        .expect("set canonical");

    db.mark_file_phase(canonical_id, phase).expect("canonical phase");
    db.mark_file_phase(duplicate_id, phase).expect("duplicate phase");

    (canonical_id, duplicate_id)
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
