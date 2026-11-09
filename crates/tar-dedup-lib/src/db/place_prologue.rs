use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use crate::common::utf8::require_utf8;
use crate::db::common::SqlFileRow;
use crate::db::common::generate_archive_and_extract_filter;
use crate::db::errors::insert_errors_in;
use crate::db::errors::{ErrorPhase, RecordDraft};
use crate::db::flags::ErrorFlags;
use crate::db::meta;
use crate::db::types::{FileId, NewOutTreeRow, OutTreeId};
use crate::error::{FileStatError, Result, ToPanic};
use rusqlite::Error::SqliteFailure;
use rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE;
use rusqlite::{Connection, named_params};

pub fn placement_prologue_done(conn: &Connection) -> Result<bool> {
    Ok(meta::get_placement_prologue_done(conn)?.unwrap_or(false))
}

pub fn set_placement_prologue_done(conn: &Connection) -> Result<()> {
    meta::set_placement_prologue_done(conn, true)
}

pub fn out_tree_is_built(conn: &Connection) -> Result<bool> {
    Ok(meta::get_out_tree_built(conn)?.unwrap_or(false))
}

pub fn set_out_tree_built(conn: &Connection) -> Result<()> {
    meta::set_out_tree_built(conn, true)
}

/// Function iterates through the files table to find all entries which should get materialized
/// based on filtering (include, exclude filter) and on file type. Additionally, a source can be
/// added s.t. only the files which are covered by this source are returned. When
/// `only_valid_new_name` is set, only rows carrying a non-empty `new_name` are returned.
/// When `apply_filters` is false the archive/extract filter gate is dropped (`--overwrite-all-dir`
/// directory pass must see excluded directories so their metadata can be restored).
pub fn list_materialized_entries<R: SqlFileRow>(
    conn: &Connection,
    last_id: FileId,
    batch_size: u64,
    source_id: Option<i64>,
    only_dirs: Option<bool>,
    only_valid_new_name: bool,
    apply_filters: bool)
    -> Result<Vec<R>> {

    debug_assert!(last_id.0 >= 0, "PRECONDITION FAILED: last_id must always be >= 0");
    let columns = R::sql_columns(Some("f"));
    let filter_dir = match only_dirs {
        None => "",
        Some(true) => " AND f.ftype = 'dir' ",
        // Everything that is not a directory (incl. `unknown`), mirroring the
        // single-pass (`None`) coverage.
        Some(false) => " AND f.ftype != 'dir' ",
    };
    let filter_new_name = if only_valid_new_name {
        " AND f.new_name IS NOT NULL AND f.new_name != '' "
    } else {
        ""
    };
    // `AND`-prefixed gate; empty when filters are bypassed (`--overwrite-all-dir`
    // directory pass must see excluded directories so their metadata can be
    // restored).
    let filter_include_exclude = if apply_filters {
        format!("AND {}", generate_archive_and_extract_filter(Some("f")))
    } else {
        String::new()
    };

    let (sql, params) = match source_id {
        Some(sid) => {
            (
                format!("
                    SELECT {columns}
                    FROM files f
                    WHERE f.id > :last_id
                        AND f.id IN (SELECT file_id FROM ref WHERE source_id = :source_id)
                        {filter_include_exclude}
                        {filter_dir}
                        {filter_new_name}
                    ORDER BY f.id
                    LIMIT :batch_size"
            ),
                 named_params! {
                     ":last_id": last_id.0,
                     ":source_id": sid.clone(),
                     ":batch_size": batch_size,
            })
        },
        None => {
            (
                format!("
                    SELECT {columns}
                    FROM files f
                    WHERE f.id > :last_id
                        {filter_include_exclude}
                        {filter_dir}
                        {filter_new_name}
                    ORDER BY f.id
                    LIMIT :batch_size"
            ),
                named_params! {
                    ":last_id": last_id.0,
                    ":batch_size": batch_size,
            })
        },
    };
    let mut stmt = conn.prepare(&sql).to_panic()?;
    let rows = stmt.query_map(
        params,
        |r| R::from_row(r, Some("f"))).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// Record the member-relative `--strip-components` result on a file row.
/// `None` clears the column (no rename applies); `Some("")` marks the empty-name
/// skip sentinel consumed by the out_tree build.
pub fn set_file_new_name(conn: &Connection, file_id: FileId, new_name: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE files SET new_name = :new_name WHERE id = :id",
        named_params! {
            ":new_name": new_name,
            ":id" : file_id.0
        },
    ).to_panic()?;
    Ok(())
}

/// Insert a new OutTree row into the table. Function then returns the ids of all rows inserted
/// based on the abs_path
/// Insert out_tree rows, recording a `MaterializeConflict` error when a row
/// maps onto an `abs_path` already owned by a *different* member (e.g. two
/// members collapsed onto one target by `--transform`/`--strip-components`/
/// relative mapping). Same-member re-mapping (`--no-strict-separation` fanout)
/// and scaffold dir rows (`file_id IS NULL`) are benign and stay silent.
/// `enabled` gates the error recording (`--no-errors`).
///
/// The conflict is surfaced by the plain `INSERT`'s UNIQUE violation on
/// `abs_path` — the observation that a row could not be added and the error
/// record happen in the same transaction, so they can never drift apart.
pub fn insert_out_tree_rows(
    conn: &mut Connection,
    rows: &[NewOutTreeRow],
    enabled: bool)
    -> Result<Vec<OutTreeId>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let tx = conn.transaction().to_panic()?;
    let mut insert = tx.prepare("
        INSERT INTO out_tree (abs_path, file_id, flags)
        VALUES (:abs_path, :file_id, :flags)").to_panic()?;

    let mut ids = Vec::with_capacity(rows.len());
    let mut conflicts: Vec<RecordDraft> = Vec::new();

    for row in rows {
        let abs = require_utf8(&row.abs_path)?;
        match insert.execute(named_params! {
            ":abs_path": abs,
            ":file_id": row.file_id.map(|id| id.0),
            ":flags": row.flags.to_i64(),
        }) {
            Ok(_) => {
                let id: i64 = tx.query_row(
                    "SELECT id FROM out_tree WHERE abs_path = :abs_path",
                    named_params! { ":abs_path": abs },
                    |r| r.get(0),
                ).to_panic()?;
                ids.push(OutTreeId(id));
            }
            Err(SqliteFailure(e, _)) if e.extended_code == SQLITE_CONSTRAINT_UNIQUE => {

                // A row already owns this abs_path.
                let (resident_id, resident_file): (i64, Option<i64>) = tx.query_row(
                    "SELECT id, file_id FROM out_tree WHERE abs_path = :abs_path",
                    named_params! { ":abs_path": abs },
                    |r| Ok((r.get(0)?, r.get(1)?)),
                ).to_panic()?;
                ids.push(OutTreeId(resident_id));
                if let (true, Some(displaced)) = (enabled, row.file_id) {
                    if resident_file != Some(displaced.0) {
                        conflicts.push(RecordDraft {
                            file_id: Some(displaced),
                            out_tree_id: Some(OutTreeId(resident_id)),
                            phase: ErrorPhase::Extract(
                                crate::config::ExtractPipelinePhase::PlacementPrologue),
                            error: FileStatError::MaterializeConflict {
                                path: row.abs_path.clone(),
                                details: format!(
                                    "member {} conflicts with resident {} at {}",
                                    displaced.0,
                                    resident_file.map_or(
                                        "<dir scaffold>".to_string(),
                                        |f| f.to_string()),
                                    row.abs_path.display(),
                                ),
                            },
                            flags: ErrorFlags::default(),
                        });
                    }
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    drop(insert);
    insert_errors_in(&tx, &conflicts)?;
    tx.commit().to_panic()?;
    Ok(ids)
}

/// Longest common path prefix (component-wise) across all `out_tree.abs_path`
/// values, relative to `root`. Returns the shared prefix as a `PathBuf` (the
/// components between `root` and the common ancestor of every entry); `None`
/// when nothing below `root` is shared (all entries live directly at the root,
/// or the tree is empty) — the `--strip-common-prefix` no-op case.
///
/// SQL-first: the whole-table scan is pushed into SQLite, which returns only the
/// lexicographic `MIN`/`MAX` `abs_path`. The component-wise common prefix of a
/// set equals the common prefix of its min and max (every other row sorts
/// between them, so it agrees with both on their shared leading segments), so
/// only those two rows are compared in Rust.
pub fn out_tree_common_prefix(conn: &Connection, root: &Path) -> Result<Option<PathBuf>> {
    let (min, max): (Option<String>, Option<String>) = conn.query_row(
        "SELECT MIN(abs_path), MAX(abs_path) FROM out_tree WHERE id > 0",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).to_panic()?;
    let (Some(min), Some(max)) = (min, max) else {
        return Ok(None);
    };
    let root_str = root.to_string_lossy();

    let rel_of = |p: &str| -> Option<Vec<OsString>> {
        let rel = p.strip_prefix(&*root_str)?;
        Some(Path::new(rel).components().filter_map(|c| match c {
            Component::Normal(n) => Some(n.to_os_string()),
            _ => None,
        }).collect())
    };
    let (Some(lo), Some(hi)) = (rel_of(&min), rel_of(&max)) else {
        return Ok(None);
    };

    let n = lo.len().min(hi.len());
    let mut i = 0;
    while i < n && lo[i] == hi[i] {
        i += 1;
    }
    if i == 0 {
        return Ok(None);
    }
    let mut out = PathBuf::new();
    for name in &lo[..i] {
        out.push(name);
    }
    Ok(Some(out))
}

/// Re-root every `out_tree` row that lives under `root / shared` back to `root`
/// and drop the row that collapses to the root itself (the source's top
/// directory). One transaction: the `UPDATE` rewrites `abs_path = root + (rel
/// remainder after `shared`)` for rows under the prefix; the `DELETE` removes
/// the entry that now equals `root`. Returns the number of re-rooted rows.
pub fn strip_common_prefix(conn: &mut Connection, root: &Path, shared: &Path) -> Result<u64> {
    let root_str = root.to_string_lossy().into_owned();
    // `lcp` = root + "/" + shared, the exact prefix of every affected row.
    let lcp = format!("{root_str}/{}", shared.to_string_lossy());
    let tx = conn.transaction().to_panic()?;
    let updated = tx.execute(
        "UPDATE out_tree
         SET abs_path = :root || substr(abs_path, length(:lcp) + 1)
         WHERE abs_path != :root
           AND substr(abs_path, 1, length(:lcp)) = :lcp
           AND (length(abs_path) = length(:lcp)
                OR substr(abs_path, length(:lcp) + 1, 1) = '/')",
        named_params! {
            ":root": &root_str,
            ":lcp": &lcp,
        },
    ).to_panic()?;
    tx.execute(
        "DELETE FROM out_tree WHERE abs_path = :root",
        named_params! { ":root": &root_str },
    ).to_panic()?;
    tx.commit().to_panic()?;
    Ok(updated as u64)
}

/// Drop every `out_tree` row whose `abs_path` equals the extraction root. Used
/// after `ensure_parent` to make sure the re-rooted tree never carries a row for
/// the effective root itself — `ensure_parent` can re-notion it (a re-rooted file
/// lands directly under the root, so the root is the parent), and we must not
/// apply archived metadata / owners to the extraction root. Returns rows dropped.
pub fn delete_out_tree_root(conn: &Connection, root: &Path) -> Result<u64> {
    let n = conn.execute(
        "DELETE FROM out_tree WHERE abs_path = :root",
        named_params! { ":root": root.to_string_lossy().into_owned() },
    ).to_panic()?;
    Ok(n as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::flags::OutTreeFlags;
    use crate::db::types::StrippedRecord;

    /// Real schema — `list_materialized_entries` reads the StrippedRecord columns
    /// plus the four filter-reason columns the gate uses.
    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::schema::initialize(&conn).unwrap();
        // Internal include sentinel referenced by `files.include_reason_*` (-1).
        for table in ["archive", "extract"] {
            conn.execute(
                &format!(
                    "INSERT OR IGNORE INTO filter_reason_{table} (id, source, line, expression) \
                     VALUES (-1, 'internal', NULL, '.*')"
                ),
                [],
            ).unwrap();
        }
        conn
    }

    /// Insert a row; include reasons are `-1` (included) so the archive+extract
    /// gate passes when `apply_filters` is set.
    fn insert_row(conn: &Connection, id: i64, abs_path: &str, ftype: &str) {
        conn.execute(
            "INSERT INTO files \
             (id, abs_path, abs_path_raw, ftype, ext, size, include_reason_archive, include_reason_extract) \
             VALUES (:id, :abs_path, :abs_path_raw, :ftype, '', 0, -1, -1)",
            named_params! {
                ":id": id,
                ":abs_path": abs_path,
                ":abs_path_raw": abs_path.as_bytes().to_vec(),
                ":ftype": ftype,
            },
        ).unwrap();
    }

    fn exclude_extract(conn: &Connection, id: i64) {
        conn.execute(
            "INSERT OR IGNORE INTO filter_reason_extract (id, source, line, expression) \
             VALUES (3, 'test', NULL, 'x')",
            [],
        ).unwrap();
        conn.execute(
            "UPDATE files SET exclude_reason_extract = 3 WHERE id = :id",
            named_params! { ":id": id },
        ).unwrap();
    }

    fn ids(conn: &Connection, only_dirs: Option<bool>, apply_filters: bool) -> Vec<i64> {
        let rows = list_materialized_entries::<StrippedRecord>(
            conn, FileId(0), 100, None, only_dirs, false, apply_filters
        )
        .unwrap();
        rows.iter().map(|r| r.id.0).collect::<Vec<_>>()
    }

    #[test]
    fn dirs_pass_bypasses_filters_for_dirs() {
        let conn = conn();
        insert_row(&conn, 1, "/a", "dir");
        insert_row(&conn, 2, "/b", "dir");
        exclude_extract(&conn, 2);
        // --overwrite-all-dir: excluded dir still materialized.
        let dirs = ids(&conn, Some(true), false);
        assert_eq!(dirs, vec![1i64, 2]);
    }

    #[test]
    fn dirs_pass_is_filter_aware_without_flag() {
        let conn = conn();
        insert_row(&conn, 1, "/a", "dir");
        insert_row(&conn, 2, "/b", "dir");
        exclude_extract(&conn, 2);
        let dirs = ids(&conn, Some(true), true);
        assert_eq!(dirs, vec![1i64]);
    }

    #[test]
    fn rest_pass_still_filters() {
        let conn = conn();
        insert_row(&conn, 1, "/a", "file");
        insert_row(&conn, 2, "/b", "file");
        exclude_extract(&conn, 2);
        let rest = ids(&conn, Some(false), true);
        assert_eq!(rest, vec![1i64]);
    }

    #[test]
    fn unknown_rows_land_in_rest_pass() {
        let conn = conn();
        insert_row(&conn, 1, "/a", "dir");
        insert_row(&conn, 2, "/u", "unknown");
        insert_row(&conn, 3, "/f", "file");
        let rest = ids(&conn, Some(false), true);
        assert_eq!(rest, vec![2i64, 3]);
    }

    #[test]
    fn materialize_conflict_recorded_and_gated_by_no_errors() {
        for enabled in [true, false] {
            let mut conn = conn();
            // Member 1 owns `/ed/target.txt` first; member 2 is displaced onto
            // the same abs_path (the transform/strip collision case).
            conn.execute(
                "INSERT INTO files (id, abs_path, abs_path_raw, ftype, ext, size) \
                 VALUES (1, '/ed/target.txt', '/ed/target.txt', 'file', 'txt', 0)",
                [],
            ).unwrap();
            conn.execute(
                "INSERT INTO files (id, abs_path, abs_path_raw, ftype, ext, size) \
                 VALUES (2, '/ed/other.txt', '/ed/other.txt', 'file', 'txt', 0)",
                [],
            ).unwrap();
            let flags = OutTreeFlags::default();
            let rows = vec![
                NewOutTreeRow { abs_path: "/ed/target.txt".into(), file_id: Some(FileId(1)), flags: flags.clone() },
                NewOutTreeRow { abs_path: "/ed/target.txt".into(), file_id: Some(FileId(2)), flags: flags.clone() },
            ];
            let ids = insert_out_tree_rows(&mut conn, &rows, enabled).expect("insert");
            // Both rows resolve to the resident (first) row; the second member is displaced.
            assert_eq!(ids, vec![OutTreeId(1), OutTreeId(1)]);

            let out_rows: i64 = conn.query_row(
                "SELECT COUNT(*) FROM out_tree", [], |r| r.get(0)).unwrap();
            assert_eq!(out_rows, 1, "single resident row");

            let err_rows: i64 = conn.query_row(
                "SELECT COUNT(*) FROM errors", [], |r| r.get(0)).unwrap();
            assert_eq!(err_rows, if enabled { 1 } else { 0 }, "recording honors no_errors");

            if enabled {
                let (file_id, abs, et): (i64, String, String) = conn.query_row(
                    "SELECT file_id, abs_path, error_type FROM errors",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                ).unwrap();
                assert_eq!(file_id, 2, "displaced member is linked");
                assert_eq!(abs, "/ed/target.txt", "conflicting target is the io path");
                assert_eq!(et, "MaterializeConflict", "kind stored as error_type");
            }
        }
    }

    #[test]
    fn same_file_fanout_stays_silent() {
        let mut conn = conn();
        conn.execute(
            "INSERT INTO files (id, abs_path, abs_path_raw, ftype, ext, size) \
             VALUES (1, '/ed/t.txt', '/ed/t.txt', 'file', 'txt', 0)",
            [],
        ).unwrap();
        let flags = OutTreeFlags::default();
        let rows = vec![
            NewOutTreeRow { abs_path: "/ed/t.txt".into(), file_id: Some(FileId(1)), flags: flags.clone() },
            NewOutTreeRow { abs_path: "/ed/t.txt".into(), file_id: Some(FileId(1)), flags: flags.clone() },
        ];
        let ids = insert_out_tree_rows(&mut conn, &rows, true).expect("insert");
        assert_eq!(ids, vec![OutTreeId(1), OutTreeId(1)]);
        let err_rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM errors", [], |r| r.get(0)).unwrap();
        assert_eq!(err_rows, 0, "same-file fanout is benign even with recording enabled");
    }

    // ---- out_tree_common_prefix / strip_common_prefix ------------------------

    fn insert_out(db: &Connection, abs_path: &str) {
        db.execute(
            "INSERT INTO out_tree (abs_path, file_id) VALUES (:abs, NULL)",
            named_params! { ":abs": abs_path },
        ).unwrap();
    }

    fn out_paths(db: &Connection) -> Vec<String> {
        // `id > 0` skips the reserved id=0 "missing link target" marker row.
        db.prepare("SELECT abs_path FROM out_tree WHERE id > 0 ORDER BY abs_path").unwrap()
            .query_map([], |r| r.get(0)).unwrap()
            .collect::<rusqlite::Result<Vec<String>>>().unwrap()
    }

    #[test]
    fn common_prefix_with_shared_subdir() {
        let db = conn();
        insert_out(&db, "/ed/var/cache/one.txt");
        insert_out(&db, "/ed/var/cache/sub/two.txt");
        insert_out(&db, "/ed/var/cache/sub/deep/three.txt");
        let shared = out_tree_common_prefix(&db, Path::new("/ed")).expect("ok")
            .expect("some shared prefix");
        assert_eq!(shared, PathBuf::from("var/cache"));
    }

    #[test]
    fn common_prefix_none_when_divergent_or_empty() {
        let db = conn();
        insert_out(&db, "/ed/a.txt");
        insert_out(&db, "/ed/b.txt");
        assert_eq!(out_tree_common_prefix(&db, Path::new("/ed")).expect("ok"), None,
                   "no shared component below root -> no-op");

        let db = conn();
        assert_eq!(out_tree_common_prefix(&db, Path::new("/ed")).expect("ok"), None,
                   "empty out_tree -> no-op");
    }

    /// Brute-force component-wise LCP over the whole table — the reference that
    /// `out_tree_common_prefix`'s MIN/MAX shortcut must agree with.
    fn true_common_prefix(db: &Connection, root: &Path) -> Option<PathBuf> {
        let paths: Vec<String> = db.prepare("SELECT abs_path FROM out_tree WHERE id > 0")
            .unwrap().query_map([], |r| r.get(0)).unwrap()
            .collect::<rusqlite::Result<Vec<_>>>().unwrap();
        let root_str = root.to_string_lossy();
        let comps = |p: &str| -> Vec<String> {
            Path::new(p.strip_prefix(&*root_str).expect("under root")).components()
                .filter_map(|c| match c {
                    Component::Normal(n) => Some(n.to_string_lossy().into_owned()),
                    _ => None,
                }).collect()
        };
        let mut shared: Option<Vec<String>> = None;
        for p in paths {
            let c = comps(&p);
            shared = Some(match shared {
                None => c,
                Some(cur) => {
                    let n = cur.len().min(c.len());
                    let mut i = 0;
                    while i < n && cur[i] == c[i] { i += 1; }
                    cur[..i].to_vec()
                }
            });
        }
        let shared = shared?;
        if shared.is_empty() { return None; }
        let mut out = PathBuf::new();
        for s in shared { out.push(s); }
        Some(out)
    }

    /// Cross-check the MIN/MAX shortcut against a brute-force full scan on sets
    /// whose min/max share a *longer* prefix than some middle element (the worry
    /// "could a whole subtree be skipped?").
    #[test]
    fn common_prefix_min_max_agrees_with_full_scan() {
        let cases: Vec<(&[&str], Option<&str>)> = vec![
            // user's example: all share `a`; min/max diverge at the 2nd comp.
            (&["/ed/a/b/c", "/ed/a/u/d"], Some("a")),
            // `net/...` breaks the `mnt` prefix entirely -> no-op.
            (&["/ed/mnt/drive/a", "/ed/mnt/hdd/b", "/ed/net/c"], None),
            // `y` sibling breaks `x` only if present.
            (&["/ed/x/q", "/ed/x/r/f", "/ed/x/t"], Some("x")),
            (&["/ed/x/q", "/ed/x/r/f", "/ed/y/z"], None),
            // deep leaves all under one subtree -> the whole subtree is kept.
            (&["/ed/mnt/drive/a", "/ed/mnt/drive/sub/b", "/ed/mnt/drive/c"], Some("mnt/drive")),
            // a single component shared.
            (&["/ed/only/deep/leaf", "/ed/only/other"], Some("only")),
        ];
        for (paths, expect) in cases {
            let mut db = conn();
            for p in paths { insert_out(&db, p); }
            let via_minmax = out_tree_common_prefix(&db, Path::new("/ed")).expect("ok");
            let brute = true_common_prefix(&db, Path::new("/ed"));
            assert_eq!(via_minmax, brute, "MIN/MAX must equal full-scan LCP for {paths:?}");
            assert_eq!(via_minmax.as_deref().map(|p| p.to_string_lossy().into_owned()),
                       expect.map(str::to_string),
                       "expected prefix for {paths:?}");
        }
    }

    #[test]
    fn common_prefix_single_component_shared() {
        let db = conn();
        insert_out(&db, "/ed/mnt/drive/a.pdf");
        insert_out(&db, "/ed/mnt/hdd/b.pdf");
        let shared = out_tree_common_prefix(&db, Path::new("/ed")).expect("ok")
            .expect("some shared prefix");
        assert_eq!(shared, PathBuf::from("mnt"));
    }

    #[test]
    fn strip_common_prefix_reroots_and_drops_top() {
        let mut db = conn();
        insert_out(&db, "/ed/var/cache/one.txt");
        insert_out(&db, "/ed/var/cache/two.txt");
        insert_out(&db, "/ed/var/cache/foo/three.txt");
        insert_out(&db, "/ed/var/cache"); // the collapsed top dir row -> root
        let n = strip_common_prefix(&mut db, Path::new("/ed"), Path::new("var/cache")).expect("ok");
        assert_eq!(n, 4, "all four rows re-rooted");
        let mut paths = out_paths(&db);
        paths.sort();
        assert_eq!(paths, vec!["/ed/foo/three.txt", "/ed/one.txt", "/ed/two.txt"]);
    }

    #[test]
    fn strip_common_prefix_only_touches_rows_under_prefix() {
        let mut db = conn();
        insert_out(&db, "/ed/mnt/drive/a.pdf");
        insert_out(&db, "/ed/other/b.pdf"); // outside the shared prefix -> untouched
        let n = strip_common_prefix(&mut db, Path::new("/ed"), Path::new("mnt/drive")).expect("ok");
        assert_eq!(n, 1);
        let mut paths = out_paths(&db);
        paths.sort();
        assert_eq!(paths, vec!["/ed/a.pdf", "/ed/other/b.pdf"]);
    }
}
