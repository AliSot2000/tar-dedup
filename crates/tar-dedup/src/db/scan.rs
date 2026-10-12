use crate::common::utf8::require_utf8;
use crate::config::ExtractRuntimeState;
use crate::db::common::{
    generate_archive_and_extract_filter, generate_archive_filter, with_transaction,
};
use crate::db::content_id::parse_content_id;
use crate::db::extract::{load_extract_runtime_state, save_extract_runtime_state};
use crate::db::flags::FileFlag;
use crate::db::types::FileId;
use crate::db::{ExtractScanState, flags, meta};
use crate::error::{Error, Result, ToPanic};
use rusqlite::{Connection, named_params};
use std::fs;
use std::path::Path;

/// Iterate over the extraction dir twice to check for files we missed and for files that are missing
pub fn check_cached_payloads(conn: &mut Connection, cache_dir: &Path) -> Result<(u64, u64)> {
    let err_cap = |e| Error::io(cache_dir, e);
    let result = if cache_dir.is_dir() {
        with_transaction(conn, |int_conn| {
            let mut new = 0u64;
            let mut present = 0u64;
            for entry in fs::read_dir(cache_dir).map_err(err_cap)? {
                let entry = entry.map_err(err_cap)?;
                let ft = entry.file_type().map_err(|e| Error::io(&entry.path(), e))?;
                if !ft.is_file() {
                    continue;
                }
                let name = entry.file_name();
                let Some(name) = name.to_str() else {
                    continue;
                };
                let Ok((_, _, file_id, _)) = parse_content_id(name) else {
                    continue;
                };
                if flags::get_file_flag(int_conn, file_id, FileFlag::FileExtracted)? {
                    present += 1;
                } else {
                    flags::set_file_flag(int_conn, file_id, FileFlag::FileExtracted, true)?;
                    new += 1;
                }
            }
            Ok((present, new))
        })?
    } else {
        (0, 0)
    };
    Ok(result)
}

/// End-of-scan salvage: promote every `FileExtracted` canonical (and dependents)
/// that is still `archived` → `unarchived`. Used when `from_footer || force`.
pub fn promote_extracted_to_unarchived(conn: &Connection) -> Result<u64> {
    let bit = FileFlag::FileExtracted.mask_i64();
    let n = conn.execute(
        "UPDATE files
         SET phase = 'unarchived'
         WHERE phase = 'archived'
           AND (
                 (flags & :bit) != 0
              OR canonical_id IN (
                     SELECT id FROM files WHERE (flags & :bit) != 0
                 )
           )",
        named_params! { ":bit": bit },
    ).to_panic()?;
    Ok(n as u64)
}

/// Triage of every row still outside `unarchived` at the end of the scan loop.
/// See `plans/scan-end-unarchived.md` for the bucket semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ScanRemainderTally {
    /// Bucket A: candidates never confirmed by a final snapshot/footer (`staged`).
    pub staged: u64,
    /// Bucket B: `archived` rows whose payload copy failed (`ErrorWhileExtracting`).
    pub extracting_err: u64,
    /// Bucket C: `archived` rows marked added whose payload was never found in
    /// the stream (no error recorded, not extract-filter-excluded).
    pub missing_member: u64,
    /// Bucket D: `archived` rows excluded by the extract filter (intentional).
    pub filter_excluded: u64,
}

impl ScanRemainderTally {
    /// Buckets that are real violations (everything but D).
    pub fn violations(&self) -> u64 {
        self.staged + self.extracting_err + self.missing_member
    }

    /// All rows outside `unarchived`, including the intentional bucket D.
    pub fn total(&self) -> u64 {
        self.violations() + self.filter_excluded
    }
}

/// Partition the rows still outside `unarchived` by cause. `eager_filter`
/// enables the extract-filter-excluded bucket (only in eager mode did the scan
/// gate skip payload members by the extract filter).
pub fn tally_scan_remainders(conn: &Connection, eager_filter: bool) -> Result<ScanRemainderTally> {
    let err = FileFlag::ErrorWhileExtracting.mask_i64();
    let extracted = FileFlag::FileExtracted.mask_i64();

    let staged: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files WHERE phase = 'staged'",
        [],
        |r| r.get("count"),
    ).to_panic()?;

    let extracting_err: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE phase = 'archived' AND (flags & :err) != 0",
        named_params! { ":err": err },
        |r| r.get("count"),
    ).to_panic()?;

    // C excludes rows the extract filter intentionally skipped (D).
    let filter_ok = if eager_filter {
        format!("AND {}", generate_archive_and_extract_filter(None))
    } else {
        String::new()
    };
    let missing_member: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) AS count FROM files
             WHERE phase = 'archived'
               AND (flags & :err) = 0
               AND (flags & :extracted) = 0
               {filter_ok}"
        ),
        named_params! {
            ":err": err,
            ":extracted": extracted,
        },
        |r| r.get("count"),
    ).to_panic()?;

    let filter_excluded: i64 = if eager_filter {
        let joint = generate_archive_and_extract_filter(None);
        conn.query_row(
            &format!(
                "SELECT COUNT(*) AS count FROM files
                 WHERE phase = 'archived' AND NOT ({joint})"
            ),
            [],
            |r| r.get("count"),
        ).to_panic()?
    } else {
        0
    };

    Ok(ScanRemainderTally {
        staged: staged as u64,
        extracting_err: extracting_err as u64,
        missing_member: missing_member as u64,
        filter_excluded: filter_excluded as u64,
    })
}

/// Promote the intentional extract-filter-excluded `archived` rows to `unarchived`
/// (they are never extracted by design). Returns the promoted count.
pub fn promote_filter_excluded_remainders(conn: &Connection) -> Result<u64> {
    let joint = generate_archive_and_extract_filter(None);
    let n = conn.execute(
        &format!(
            "UPDATE files
             SET phase = 'unarchived'
             WHERE phase = 'archived' AND NOT ({joint})"
        ),
        [],
    ).to_panic()?;
    Ok(n as u64)
}

/// Force-salvage: blanket-promote every remaining `staged`/`archived` row to
/// `unarchived` so the post-scan invariant holds. Called only by the end-of-scan
/// gate once the violation buckets have been triaged.
pub fn promote_scan_remainders(conn: &Connection) -> Result<u64> {
    let n = conn.execute(
        "UPDATE files
         SET phase = 'unarchived'
         WHERE phase IN ('staged', 'archived')",
        [],
    ).to_panic()?;
    Ok(n as u64)
}

pub fn record_snapshot_ingested(conn: &mut Connection) -> Result<u32> {
    let mut scan = load_extract_scan_state(conn)?;
    scan.snapshots_ingested = scan.snapshots_ingested.saturating_add(1);
    save_extract_scan_state(conn, &scan)?;

    // Keep ExtractRuntimeState in sync when present.
    if let Some(mut runtime) = load_extract_runtime_state(conn)? {
        runtime.snapshots_ingested = scan.snapshots_ingested;
        save_extract_runtime_state(conn, &runtime)?;
    }

    Ok(scan.snapshots_ingested)
}

// TODO rework and rethink this function.
/// Snapshot confirmation: first, `staged` candidates listed as `archived` in the
/// snapshot leave the unconfirmed manifest state (`staged` → `archived`). Then
/// promote `archived` → `unarchived` for paths the snapshot lists as archived whose
/// payload has been extracted (canonical carries [`FileFlag::FileExtracted`]),
/// fanning phase out over `canonical_id`.
pub fn apply_snapshot_promote_unarchived(
    conn: &Connection,
    snapshot_path: &Path)
    -> Result<u64> {
    let path = require_utf8(snapshot_path)?;
    let bit = FileFlag::FileExtracted.mask_i64();
    conn.execute(
        "ATTACH DATABASE :path AS snap",
        named_params! { ":path": path },
    ).to_panic()?;
    // Confirmation: a row the snapshot reports as archived was written into the
    // stream — it leaves the manifest's `staged` state.
    conn.execute(
        "UPDATE files
         SET phase = 'archived'
         WHERE phase = 'staged'
           AND abs_path IN (SELECT abs_path FROM snap.files WHERE phase = 'archived')",
        [],
    ).to_panic()?;
    let promoted = conn.execute(
        "UPDATE files
         SET phase = 'unarchived'
         WHERE phase = 'archived'
           AND abs_path IN (SELECT abs_path FROM snap.files WHERE phase = 'archived')
           AND (
                 (flags & :bit) != 0
              OR canonical_id IN (
                     SELECT id FROM files WHERE (flags & :bit) != 0
                 )
           )",
        named_params! { ":bit": bit },
    ).to_panic()?;
    // Descendants of already-unarchived canonicals.
    conn.execute(
        "UPDATE files
         SET phase = 'unarchived'
         WHERE phase = 'archived'
           AND canonical_id IS NOT NULL
           AND canonical_id IN (SELECT id FROM files WHERE phase = 'unarchived')",
        [],
    ).to_panic()?;
    conn.execute("DETACH DATABASE snap", []).to_panic()?;
    Ok(promoted as u64)
}

/// Canonical rows with `AppendedPath` but without `FileExtracted`.
pub fn count_missing_payloads(conn: &Connection) -> Result<u64> {
    let appended = FileFlag::AppendedPath.mask_i64();
    let extracted = FileFlag::FileExtracted.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE canonical_id = id
           AND (flags & :appended) != 0
           AND (flags & :extracted) = 0",
        named_params! {
            ":appended": appended,
            ":extracted": extracted,
        },
        |row| row.get("count"),
    ).to_panic()?;
    Ok(count as u64)
}

/// Rows still `archived` whose canonical has `FileExtracted` (awaiting confirmation).
pub fn count_unconfirmed_extracted(conn: &Connection) -> Result<u64> {
    let bit = FileFlag::FileExtracted.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE phase = 'archived'
           AND (
                 (flags & :bit) != 0
              OR canonical_id IN (
                     SELECT id FROM files WHERE (flags & :bit) != 0
                 )
           )",
        named_params! { ":bit": bit },
        |row| row.get("count"),
    ).to_panic()?;
    Ok(count as u64)
}

/// Canonical rows carrying `FileExtracted`.
pub fn count_extracted_canonical(conn: &Connection) -> Result<u64> {
    let bit = FileFlag::FileExtracted.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE canonical_id = id AND (flags & :bit) != 0",
        named_params! { ":bit": bit },
        |row| row.get("count"),
    ).to_panic()?;
    Ok(count as u64)
}

/// All rows in canonical groups that have `FileExtracted` on the canonical.
pub fn count_extracted_paths(conn: &Connection) -> Result<u64> {
    let bit = FileFlag::FileExtracted.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE (flags & :bit) != 0
            OR canonical_id IN (
                   SELECT id FROM files WHERE (flags & :bit) != 0
               )",
        named_params! { ":bit": bit },
        |row| row.get("count"),
    ).to_panic()?;
    Ok(count as u64)
}

/// `(ftype_label, count)` for rows that lack `AppendedPath` which aren't file. Also excludes the
/// number of filter_excluded files
pub fn count_non_appended_by_ftype(conn: &Connection) -> Result<Vec<(String, u64)>> {
    let bit = FileFlag::AppendedPath.mask_i64();
    let mut stmt = conn.prepare(&format!(
        "SELECT ftype AS ft, COUNT(*) AS count
         FROM files
         WHERE (flags & :bit) = 0
            AND {}
         GROUP BY ftype
         ORDER BY ft",
        crate::db::common::generate_archive_filter(None),
    )).to_panic()?;
    let rows = stmt.query_map(
        named_params! { ":bit": bit },
        |row| {
            Ok((
                row.get::<_, String>("ft")?,
                row.get::<_, i64>("count")? as u64,
            ))
    }).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// Determine if this canonical_id is required by a filtered value
pub fn should_extract_canonical_id(conn: &Connection, id: FileId) -> Result<bool>{
    let filter_cols = crate::db::common::generate_archive_and_extract_filter(None);
    let res = conn.query_row(&format!(
        "SELECT COUNT(*) AS count
        FROM files
        WHERE flags & :archived
            AND ftype = 'file'
            AND {filter_cols}
            AND canonical_id = :id"),
        named_params! {
            ":archived": FileFlag::AppendedPath.mask_i64(),
            ":id": id.0
        },
        |row| row.get::<_, i64>("count")
    ).to_panic()?;
    Ok(res > 0)
}

// TODO this should not technically be necessary. The we should be able to run the integrity checks
//  when extracting
/// Normalize a freshly installed catalog so stream handling is provenance-agnostic.
/// Clears [`FileFlag::FileExtracted`]; promotes every scan-phase **non-participant**
/// straight to `unarchived` (dup / non-file / sha-less / archive-filter-failed rows,
/// and anything the archive pipeline left before `staged`); the scan-phase
/// **participants** (self-canonical `file` with digest passing the archive filter)
/// keep their install phase — `staged` (leading-manifest provenance, awaiting a
/// confirmation snapshot) or `archived` (footer provenance) — so a payload that is
/// present-but-unconfirmed stays visible and attributable at the end of the scan.
/// Must not run on a resumed work DB.
pub fn normalize_installed_catalog(conn: &mut Connection) -> Result<()> {
    let bit = FileFlag::FileExtracted.mask_i64();
    conn.execute(
        "UPDATE files SET flags = flags & ~:bit",
        named_params! { ":bit": bit },
    ).to_panic()?;
    // Anything the archive pipeline left mid-flight can never become a payload
    // member; it is not a participant of the scan phase → unarchived directly.
    conn.execute(
        "UPDATE files
         SET phase = 'unarchived'
         WHERE phase IN (
             'inventoried', 'hashed', 'filtered', 'deduped', 'sparsified'
         )",
        [],
    ).to_panic()?;
    // `archived` rows: keep extractable participants, sweep the rest.
    let filter = generate_archive_filter(None);
    conn.execute(&format!(
        "UPDATE files
         SET phase = 'unarchived'
         WHERE phase = 'archived'
           AND (
             canonical_id IS NULL
             OR canonical_id != id
             OR ftype != 'file'
             OR sha1 IS NULL
             OR NOT ({filter})
           )"
    ), []).to_panic()?;
    Ok(())
}

/// Copy an embedded catalog into the extract work DB.
pub fn install_initial_manifest(snapshot_path: &Path, db_path: &Path) -> Result<()> {
    if db_path.is_file() {
        fs::remove_file(db_path).map_err(|e| Error::io(db_path, e))?;
    }
    fs::copy(snapshot_path, db_path).map_err(|e| Error::io(db_path, e))?;
    Ok(())
}

pub fn save_extract_scan_state(conn: &mut Connection, state: &ExtractScanState)
    -> Result<()> {
    with_transaction(conn, |conn| {
        meta::set_scan_tar_saw_manifest_db(conn, state.saw_manifest_db)?;
        meta::set_scan_tar_saw_any_members(conn, state.saw_any_members)?;
        meta::set_scan_tar_complete(conn, state.scan_complete)?;
        meta::set_scan_tar_last_member_index(conn, state.last_member_index)?;
        meta::set_scan_tar_from_footer(conn, state.from_footer)?;
        meta::set_scan_tar_last_member_was_snapshot(conn, state.last_member_was_snapshot)?;
        meta::set_extract_snapshots_ingested(conn, state.snapshots_ingested)?;
        Ok(())
    })
}

pub fn init_extract_runtime_state(conn: &mut Connection) -> Result<()> {
    if load_extract_runtime_state(conn)?.is_none() {
        save_extract_runtime_state(conn, &ExtractRuntimeState::new())?;
    }
    Ok(())
}

/// The two nt variables are populated for the run as follows:
/// nt_snapshot_before = snapshots_ingested
/// nt_stopped = false
pub fn load_extract_scan_state(conn: &Connection) -> Result<ExtractScanState> {
    let snapshot_ingested = meta::get_extract_snapshots_ingested(conn)?.unwrap_or(0);
    Ok(ExtractScanState {
        saw_manifest_db: meta::get_scan_tar_saw_manifest_db(conn)?.unwrap_or(false),
        saw_any_members: meta::get_scan_tar_saw_any_members(conn)?.unwrap_or(false),
        scan_complete: meta::get_scan_tar_complete(conn)?.unwrap_or(false),
        last_member_index: meta::get_scan_tar_last_member_index(conn)?,
        from_footer: meta::get_scan_tar_from_footer(conn)?.unwrap_or(false),
        last_member_was_snapshot: meta::get_scan_tar_last_member_was_snapshot(conn)?
            .unwrap_or(false),
        snapshots_ingested: snapshot_ingested,

        nt_snapshot_before: snapshot_ingested,
        nt_stopped: false,
    })
}

//==================================================================================================
// Testing — normalize / snapshot-confirm / end-of-scan remainder buckets.
//==================================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use std::path::PathBuf;

    fn open_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = Connection::open(&dir.path().join("t.sqlite")).expect("open conn");
        schema::initialize(&conn).expect("schema init");
        for table in ["archive", "extract"] {
            conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO filter_reason_{table} (id, source, line, expression) \
                     VALUES (-1, 'internal', NULL, '.*'), (5, 'internal', NULL, 'exclude-test')"
                ),
                [],
            ).expect("seed filter reasons");
        }
        (dir, conn)
    }

    fn m(flag: FileFlag) -> i64 {
        flag.mask_i64()
    }

    fn file_row(
        conn: &Connection, id: i64, abs_path: &str, ftype: &str, phase: &str,
        sha1: Option<Vec<u8>>, canonical: Option<i64>, flags: i64) {
        conn.execute(
             "INSERT INTO files (id, abs_path, abs_path_raw, ext, size, ftype, phase, sha1, \
             include_reason_archive, exclude_reason_archive, \
             include_reason_extract, exclude_reason_extract, \
             canonical_id, flags) \
             VALUES (:id, :abs_path, :abs_path_raw, '.bin', 0, :ftype, :phase, :sha1, -1, 0, -1, 0, \
                     :canon, :flags)",
            named_params! {
                ":id": id,
                ":abs_path": abs_path,
                ":abs_path_raw": abs_path.as_bytes().to_vec(),
                ":ftype": ftype,
                ":phase": phase,
                ":sha1": sha1.as_deref(),
                ":canon": canonical,
                ":flags": flags,
            },
        ).expect("insert file row");
    }

    fn phase(conn: &Connection, id: i64) -> String {
        conn.query_row(
            "SELECT phase FROM files WHERE id = :id",
            named_params! { ":id": id },
            |r| r.get::<_, String>(0),
        ).expect("read phase")
    }

    fn checkpoint(conn: &Connection) {
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);").expect("checkpoint");
    }

    #[test]
    fn normalize_keeps_participants_and_promotes_non_participants() {
        let (_dir, mut conn) = open_db();
        let sha = [1u8; 20].to_vec();
        // participants: self-canonical file with digest passing the archive filter
        file_row(&conn, 1, "/ed/p1", "file", "archived", Some(sha.clone()), Some(1),
                 m(FileFlag::FileExtracted));
        file_row(&conn, 2, "/ed/p2", "file", "staged", Some(sha.clone()), Some(2), 0);
        // non-participants: dup, non-file, sha-less, archive-filter-failed, pre-stage
        file_row(&conn, 3, "/ed/d3", "file", "archived", Some(sha.clone()), Some(1), 0);
        file_row(&conn, 4, "/ed/x4", "dir", "archived", None, None, 0);
        file_row(&conn, 5, "/ed/s5", "file", "archived", None, None, 0);
        file_row(&conn, 6, "/ed/e6", "file", "archived", Some(sha.clone()), Some(6), 0);
        conn.execute("UPDATE files SET exclude_reason_archive = 5 WHERE id = 6", [])
            .expect("exclude archive");
        file_row(&conn, 7, "/ed/y7", "file", "sparsified", Some(sha.clone()), Some(7), 0);

        normalize_installed_catalog(&mut conn).expect("normalize");

        assert_eq!(phase(&conn, 1), "archived", "participant archived stays");
        assert_eq!(phase(&conn, 2), "staged", "participant staged stays");
        for id in 3..=7 {
            assert_eq!(phase(&conn, id), "unarchived", "id {id} swept to unarchived");
        }
        // FileExtracted cleared on the retained participant
        let f = flags::get_file_flags(&conn, FileId(1)).expect("flags");
        assert!(!f.get(FileFlag::FileExtracted));
    }

    #[test]
    fn snapshot_confirms_staged_then_unarchives_extracted() {
        let (_dir, conn) = open_db();
        file_row(&conn, 1, "/ed/1.cid", "file", "staged", Some([1u8; 20].to_vec()),
                 Some(1), 0);
        file_row(&conn, 2, "/ed/2.cid", "file", "staged", Some([2u8; 20].to_vec()),
                 Some(2), m(FileFlag::FileExtracted));
        // The snapshot reports both rows archived (post-archive state).
        let (_sdir, snap) = open_db();
        file_row(&snap, 1, "/ed/1.cid", "file", "archived", Some([1u8; 20].to_vec()),
                 Some(1), 0);
        file_row(&snap, 2, "/ed/2.cid", "file", "archived", Some([2u8; 20].to_vec()),
                 Some(2), 0);
        checkpoint(&snap);

        let promoted = apply_snapshot_promote_unarchived(
            &conn, &PathBuf::from(_sdir.path().join("t.sqlite")),
        ).expect("snapshot");

        assert_eq!(promoted, 1);
        assert_eq!(phase(&conn, 1), "archived", "confirmed but not extracted stays archived");
        assert_eq!(phase(&conn, 2), "unarchived", "confirmed + extracted promotes");
    }

    #[test]
    fn tally_and_promote_scan_remainders() {
        let (_dir, conn) = open_db();
        // A: staged (tail-truncated)
        file_row(&conn, 1, "/ed/a", "file", "staged", Some([1u8; 20].to_vec()), Some(1), 0);
        // B: archived + ErrorWhileExtracting
        file_row(&conn, 2, "/ed/b", "file", "archived", Some([2u8; 20].to_vec()), Some(2),
                 m(FileFlag::ErrorWhileExtracting));
        // C: archived, no flags, extract-filter-passing (member missing)
        file_row(&conn, 3, "/ed/c", "file", "archived", Some([3u8; 20].to_vec()), Some(3), 0);
        // D: archived but extract-filter-excluded
        file_row(&conn, 4, "/ed/d", "file", "archived", Some([4u8; 20].to_vec()), Some(4), 0);
        conn.execute("UPDATE files SET exclude_reason_extract = 5 WHERE id = 4", [])
            .expect("exclude extract");
        file_row(&conn, 5, "/ed/e", "file", "unarchived", Some([5u8; 20].to_vec()), Some(5),
                 m(FileFlag::FileExtracted));

        let eager = tally_scan_remainders(&conn, true).expect("tally eager");
        assert_eq!(eager.staged, 1);
        assert_eq!(eager.extracting_err, 1);
        assert_eq!(eager.missing_member, 1, "id4 is filter-excluded, not missing");
        assert_eq!(eager.filter_excluded, 1);
        assert_eq!(eager.violations(), 3);
        assert_eq!(eager.total(), 4);

        // lazy mode never consulted the extract filter during the scan → no D,
        // and the filter-excluded row lands in the missing bucket.
        let lazy = tally_scan_remainders(&conn, false).expect("tally lazy");
        assert_eq!(lazy.missing_member, 2);
        assert_eq!(lazy.filter_excluded, 0);

        // Force-salvage ordering: promote D first, then the violations.
        assert_eq!(promote_filter_excluded_remainders(&conn).expect("promote D"), 1);
        assert_eq!(phase(&conn, 4), "unarchived");
        assert_eq!(promote_scan_remainders(&conn).expect("promote sweep"), 3);
        for id in 1..=4 {
            assert_eq!(phase(&conn, id), "unarchived", "id {id} swept");
        }
        assert_eq!(phase(&conn, 5), "unarchived", "already unarchived untouched");
    }
}
