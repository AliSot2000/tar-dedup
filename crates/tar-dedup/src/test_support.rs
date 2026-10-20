//! Shared test support for the phase unit-test modules (`#[cfg(test)]` only).
//!
//! Consolidates the `TestWorld` (tempdir + DB + config + progress + shutdown)
//! and the archive config builder that the archive phase tests used to
//! duplicate per module (`hash`, `stage`, `dedup`, `sparsify`, `tar_builder`).

use std::fs;
use std::path::{Path, PathBuf};

use crate::archive::ArchiveRTArgs;
use crate::common::defaults::{SNAPSHOT_INIT_TAR_NAME, SNAPSHOT_TAR_NAME};
use crate::common::files::original_extension;
use crate::config::{
    ArchiveConfig, ArchivePipelineOptions, CaptureOptions, ExtractConfig, FilterOptions,
    IndexingOptions, ProcessOptions, SparseOptions,
};
use crate::db::Database;
use crate::db::content_id::content_id_from_digest;
use crate::db::flags::FileFlag;
use crate::db::types::{FileId, FilePhase, FileRecord, FileType, NewFileRecord, StrippedRecord};
use crate::progress::{ARCHIVE_MULTIPLIER, ProgressBarSet};
use crate::shutdown::Shutdown;
use chrono::{DateTime, Utc};
use rusqlite::named_params;

#[cfg(unix)]
use std::os::unix::fs::symlink;

/// The internal include rule the filter phase installs: without it `apply_no_filter`
/// (=-1) violates the FK. Idempotent (INSERT OR IGNORE).
pub fn seed_filter_reason(db: &Database) {
    db.with_transaction(|conn| {
        conn.execute(
            "INSERT OR IGNORE INTO filter_reason_archive (id, source, line, expression) \
             VALUES (-1, 'internal', NULL, '.*')", [],
        ).expect("seed internal include rule");
        Ok(())
    }).expect("seed internal include rule tx");
}

/// Common archive config shared by the archive phase tests: eager filter off,
/// no metadata capture, hardlink detection off, and the stage/sparsify-friendly
/// worker + sparse defaults. Phases needing different values tune via
/// [`TestWorld::tuned`] (e.g. hash's io-bound workers).
pub fn test_archive_config() -> ArchiveConfig {
    ArchiveConfig {
        indexing: IndexingOptions {
            no_hardlink_detection: true,
            ..IndexingOptions::default()
        },
        filter: FilterOptions { eager_filter: false, ..FilterOptions::default() },
        capture: CaptureOptions {
            do_xattrs: false,
            do_posix_acl: false,
            do_selinux: false,
            ..CaptureOptions::default()
        },
        sparse: SparseOptions { sparsify: true, min_pages: 4, ..SparseOptions::default() },
        process: ProcessOptions { jobs: 4, io_jobs: 1, ..ProcessOptions::default() },
        pipeline: ArchivePipelineOptions {
            max_shard_size: 0,
            write_archive_footer: false,
            ..ArchivePipelineOptions::default()
        },
        ..ArchiveConfig::default()
    }
}

/// Archive-pipeline test world: a tempdir with `{work_dir=tmp/astage,
/// archive_path=tmp/test.astar}` and a DB at `work_dir/tar-dedup.sqlite` (what
/// `db_path()` resolves to), plus a detached shutdown and the standard progress
/// bar set. All fields are `pub` for direct test access.
pub struct TestWorld {
    pub dir: tempfile::TempDir,
    pub db: Database,
    pub shutdown: Shutdown,
    pub progress: ProgressBarSet,
    pub config: ArchiveConfig,
}

impl TestWorld {
    /// Basic world (canonical [`test_archive_config`]).
    pub fn new() -> Self {
        Self::tuned(|_| {})
    }

    /// World with a phase-specific config tune applied before construction.
    /// e.g. `TestWorld::tuned(|c| c.process.io_jobs = 4)`.
    pub fn tuned(f: impl FnOnce(&mut ArchiveConfig)) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = test_archive_config();
        config.paths.work_dir = dir.path().join("astage");
        config.paths.archive_path = dir.path().join("test.astar");
        f(&mut config);
        fs::create_dir_all(&config.paths.work_dir).expect("create work dir");
        let db = Database::open(&config.paths.db_path()).expect("open db");
        seed_filter_reason(&db);
        Self {
            dir,
            db,
            shutdown: Shutdown::detached(),
            progress: ProgressBarSet::new(ARCHIVE_MULTIPLIER),
            config,
        }
    }

    pub fn rt(&self) -> ArchiveRTArgs<'_> {
        ArchiveRTArgs {
            config: &self.config,
            db: &self.db,
            shutdown: &self.shutdown,
            progress: &self.progress,
        }
    }

    pub fn rt_with<'a>(&'a self, shutdown: &'a Shutdown) -> ArchiveRTArgs<'a> {
        ArchiveRTArgs {
            config: &self.config,
            db: &self.db,
            shutdown,
            progress: &self.progress,
        }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    pub fn add_file(&self, name: &str, payload: &[u8]) -> FileId {
        self.insert_recorded(&self.path(name), payload, None, None, None)
    }

    pub fn add_file_recorded(
        &self, name: &str, payload: &[u8],
        mtime: Option<DateTime<Utc>>, atime: Option<DateTime<Utc>>, ctime: Option<DateTime<Utc>>)
        -> FileId {
        self.insert_recorded(&self.path(name), payload, mtime, atime, ctime)
    }

    /// Write `payload` to `path` and insert a matching `files` row.
    pub fn insert_recorded(
        &self, path: &Path, payload: &[u8],
        mtime: Option<DateTime<Utc>>, atime: Option<DateTime<Utc>>, ctime: Option<DateTime<Utc>>)
        -> FileId {
        std::fs::write(path, payload).expect("write test payload");
        self.db.insert_file(&NewFileRecord {
            abs_path: PathBuf::from(path),
            ext: original_extension(path),
            size: payload.len() as u64,
            mtime, atime, ctime,
            uid: None, gid: None, mode: None,
            ftype: Some(FileType::File),
            xattrs: None, posix_acl: None, selinux_ctx: None, win_perm: None, link_dst: None, abs_path_raw: None, link_dst_raw: None,
            device_id: None, inode_id: None, major: None, minor: None,
        }).expect("insert test file");
        self.db.file_id_by_abs_path(path).expect("lookup test file").expect("file present")
    }

    pub fn phase(&self, id: FileId) -> FilePhase {
        self.db.get_file_by_id::<StrippedRecord>(id)
            .expect("get row")
            .expect("row present")
            .phase
    }

    pub fn flag(&self, id: FileId, flag: FileFlag) -> bool {
        self.db.get_file_flag(id, flag).expect("get flag")
    }

    pub fn record(&self, id: FileId) -> StrippedRecord {
        self.db.get_file_by_id::<StrippedRecord>(id)
            .expect("get row")
            .expect("row present")
    }

    /// Mark `id` as `staged` canonical with filter columns set — the exact
    /// precondition both the stage and archive phases evaluate. Deeper than a
    /// bare phase flip: `canonical_id`, `sha1` and the filter columns are also
    /// pinned so the row qualifies for the stage/archive queues.
    pub fn seed_staged_row(&self, id: FileId) {
        self.db.with_transaction(|conn| {
            let n = conn.execute(
                "UPDATE files SET phase = 'staged', canonical_id = :id, \
                 sha1 = :sha1, include_reason_archive = -1, exclude_reason_archive = 0 \
                 WHERE id = :id",
                named_params! {
                    ":id": id.0,
                    ":sha1": [7u8; 20].as_slice(),
                },
            ).expect("seed staged row");
            assert_eq!(n, 1);
            Ok(())
        }).expect("seed staged row tx");
    }

    /// Absolute `{stage_dir}/{tar member}` target path for `id`.
    pub fn target_path(&self, id: FileId) -> PathBuf {
        let name = self.record(id).tar_member_name().expect("canonical");
        self.config.paths.stage_dir().join(name)
    }

    /// Create the symlink stage would create, and return the path.
    #[cfg(unix)]
    pub fn place_symlink(&self, id: FileId) -> PathBuf {
        let rec = self.record(id);
        let target = self.target_path(id);
        symlink(rec.fs_path(), &target).expect("place symlink");
        target
    }
}

impl Default for TestWorld {
    fn default() -> Self {
        Self::new()
    }
}

// -----------------------------------------------------------------------------
// Extract-side archive builders (moved from `unarchive/scan.rs` tests so the
// downstream extract phase tests can construct a scan-ready catalog).
// -----------------------------------------------------------------------------

/// Catalog with one canonical regular file and one duplicate pointing at it,
/// both `archived`, canonical flagged `AppendedPath`. Returns the member name.
pub fn write_catalog(path: &Path) -> String {
    let db = Database::open(path).expect("open catalog");
    let canonical = insert(&db, "/canonical.txt", 4, FileType::File);
    let duplicate = insert(&db, "/duplicate.txt", 4, FileType::File);

    // Seed the joint include filter (id -1 = internal catch-all) in both
    // tables so the eager-extract gate (`should_extract_canonical_id`) sees
    // the canonicals as filter-eligible; then pin the reason columns.
    db.with_transaction(|conn| {
        for table in ["archive", "extract"] {
            conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO filter_reason_{table} (id, source, line, expression) \
                     VALUES (-1, 'internal', NULL, '.*')"
                ),
                [],
            ).expect("seed internal include rule");
        }
        Ok(())
    }).expect("filter seed tx");
    for id in [canonical, duplicate] {
        db.with_transaction(|conn| {
            conn.execute(
                "UPDATE files SET include_reason_archive = -1, exclude_reason_archive = 0, \
                 include_reason_extract = -1, exclude_reason_extract = 0 WHERE id = :id",
                rusqlite::named_params! { ":id": id.0 },
            ).expect("set reason columns");
            Ok(())
        }).expect("reason tx");
    }

    db.update_file_inspection_per_id(canonical, [7u8; 20], 0, false)
        .expect("digest");
    db.mark_self_canonical(canonical).expect("self canonical");
    db.set_canonical(duplicate, canonical).expect("canonical");
    db.set_file_flag(canonical, FileFlag::AppendedPath, true)
        .expect("appended");
    db.mark_file_phase(canonical, FilePhase::Archived)
        .expect("phase");
    db.mark_file_phase(duplicate, FilePhase::Archived)
        .expect("phase");

    let member = db
        .get_file_by_id::<FileRecord>(canonical)
        .expect("get")
        .expect("row")
        .tar_member_name()
        .expect("content id");
    db.checkpoint().expect("checkpoint");
    member
}

/// Insert a `files` row and return its id (looked up by `abs_path`).
pub fn insert(db: &Database, abs_path: &str, size: u64, ftype: FileType) -> FileId {
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
        ftype: Some(ftype),
        xattrs: None,
        posix_acl: None,
        selinux_ctx: None,
        win_perm: None,
        link_dst: None,
        abs_path_raw: None,
        link_dst_raw: None,
        device_id: None,
        inode_id: None,
        major: None,
        minor: None,
    })
    .expect("insert");

    db.files_in_phase::<StrippedRecord>(FilePhase::Inventoried)
        .expect("list")
        .into_iter()
        .find(|f| f.abs_path == Path::new(abs_path))
        .expect("inserted row")
        .id
}

/// Append a regular-file tar member with the given name and bytes.
pub fn append_member(builder: &mut tar::Builder<fs::File>, name: &str, bytes: &[u8]) {
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::Regular);
    header.set_mode(0o644);
    header.set_size(bytes.len() as u64);
    header.set_cksum();
    builder.append_data(&mut header, name, bytes).expect("append");
}

/// Seed the joint include filter (id -1 = internal catch-all) in both tables
/// and pin the four reason columns for `id`.
pub fn seed_filter_reasons(db: &Database, id: FileId) {
    db.with_transaction(|conn| {
        for table in ["archive", "extract"] {
            conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO filter_reason_{table} (id, source, line, expression) \
                     VALUES (-1, 'internal', NULL, '.*')"
                ),
                [],
            ).expect("seed internal include rule");
        }
        conn.execute(
            "UPDATE files SET include_reason_archive = -1, exclude_reason_archive = 0, \
             include_reason_extract = -1, exclude_reason_extract = 0 WHERE id = :id",
            rusqlite::named_params! { ":id": id.0 },
        ).expect("set reason columns");
        Ok(())
    }).expect("filter seed tx");
}

/// A self-canonical regular file, `AppendedPath` (+`phase`) so the eager
/// extract gate and promotions treat it as an archived participant.
pub fn add_archived_canonical(db: &Database, abs_path: &str, sha: u8, phase: FilePhase) -> FileId {
    let id = insert(db, abs_path, 4, FileType::File);
    seed_filter_reasons(db, id);
    db.update_file_inspection_per_id(id, [sha; 20], 0, false).expect("digest");
    db.mark_self_canonical(id).expect("self canonical");
    db.set_file_flag(id, FileFlag::AppendedPath, true).expect("appended");
    db.mark_file_phase(id, phase).expect("phase");
    id
}

/// `manifest.sqlite`, one payload, `snapshot.sqlite` — an uncompressed archive.
pub fn build_archive(dir: &Path) -> (ExtractConfig, String) {
    let catalog = dir.join("catalog.sqlite");
    let member = write_catalog(&catalog);
    let catalog_bytes = fs::read(&catalog).expect("read catalog");

    let archive_path = dir.join("archive.tar");
    let mut builder = tar::Builder::new(fs::File::create(&archive_path).expect("create tar"));
    append_member(&mut builder, SNAPSHOT_INIT_TAR_NAME, &catalog_bytes);
    append_member(&mut builder, &member, b"data");
    append_member(&mut builder, SNAPSHOT_TAR_NAME, &catalog_bytes);
    builder.finish().expect("finish tar");

    let config = ExtractConfig::for_scan_test(
        archive_path,
        dir.join("work"),
        dir.join("out"),
    );
    (config, member)
}

/// `manifest.sqlite`, `n` payloads, `snapshot.sqlite` — an uncompressed archive
/// with `n` self-canonical archived files (`/f_{i}.txt`, distinct digest) plus one
/// duplicate pointing at the last canonical. Returns the config and the member names
/// in stream order.
pub fn build_multi_catalog(dir: &Path, n: usize) -> (ExtractConfig, Vec<String>) {
    let catalog = dir.join("catalog.sqlite");
    let db = Database::open(&catalog).expect("open catalog");
    let mut members = Vec::new();
    let mut last_id = FileId(0);
    for i in 0..n {
        let id = add_archived_canonical(&db, &format!("/f_{i}.txt"), i as u8, FilePhase::Archived);
        last_id = id;
        let member = db
            .get_file_by_id::<FileRecord>(id)
            .expect("get")
            .expect("row")
            .tar_member_name()
            .expect("content id");
        members.push(member);
    }
    let dup = insert(&db, "/dup.txt", 4, FileType::File);
    seed_filter_reasons(&db, dup);
    db.set_canonical(dup, last_id).expect("canonical");
    db.mark_file_phase(dup, FilePhase::Archived).expect("phase");
    db.checkpoint().expect("checkpoint");
    drop(db);

    let catalog_bytes = fs::read(&catalog).expect("read catalog");
    let archive_path = dir.join("archive.tar");
    let mut builder = tar::Builder::new(fs::File::create(&archive_path).expect("create tar"));
    append_member(&mut builder, SNAPSHOT_INIT_TAR_NAME, &catalog_bytes);
    for member in &members {
        append_member(&mut builder, member, b"data");
    }
    append_member(&mut builder, SNAPSHOT_TAR_NAME, &catalog_bytes);
    builder.finish().expect("finish tar");

    let config = ExtractConfig::for_scan_test(
        archive_path,
        dir.join("work"),
        dir.join("out"),
    );
    (config, members)
}

/// `build_archive` plus a seekable sqlite footer appended via
/// [`crate::archive_footer::write_footer`].
pub fn build_footer_archive(dir: &Path) -> (ExtractConfig, String) {
    let (config, member) = build_archive(dir);
    let catalog = dir.join("catalog.sqlite");
    crate::archive_footer::write_footer(&config.paths.archive_path, &catalog, None)
        .expect("write footer");
    (config, member)
}

/// Mint a tar member name for a canonical row (not necessarily present in any
/// catalog) — used to plant a *stray* file in the extract cache dir.
pub fn content_id_name(fid: FileId, sha: [u8; 20], size: u64) -> String {
    content_id_from_digest(Some(&sha), size, fid, Path::new("f.txt")).0
}

/// Flip a single byte in `path` at `offset` — the archive-corruption primitive for
/// the footer-integrity tests.
pub fn corrupt_byte(path: &Path, offset: u64) {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("open for corruption");
    f.seek(SeekFrom::Start(offset)).expect("seek");
    let mut b = [0u8; 1];
    f.read_exact(&mut b).expect("read byte");
    b[0] ^= 0xFF;
    f.seek(SeekFrom::Start(offset)).expect("seek");
    f.write_all(&b).expect("write byte");
}