//! Read-only archive/file stats for the `inspect` command.

use rusqlite::Connection;

use crate::db::flags::FileFlag;
use crate::error::{Result, ToPanic};

/// `(ftype, count)` pairs over every row in `files`.
pub fn count_by_ftype(conn: &Connection) -> Result<Vec<(String, u64)>> {
    let mut stmt = conn.prepare(
        "SELECT ftype AS ft, COUNT(*) AS count
         FROM files
         GROUP BY ftype
         ORDER BY ft",
    ).to_panic()?;
    let rows = stmt.query_map(
        [],
        |row| {
            Ok((
                row.get::<_, String>("ft")?,
                row.get::<_, i64>("count")? as u64,
            ))
        },
    ).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// Filter-bucket counts in archive-filter terms: `(included, excluded_by_include,
/// excluded_by_exclude, unfiltered)`.
///
/// Mirrors the `step_filters` bucket logic: included = `include_reason_archive < 0`,
/// excluded-by-include = `include_reason_archive = 0`, excluded-by-exclude =
/// `exclude_reason_archive > 0 AND include_reason_archive < 0`; rows never touched
/// by a filter pass carry a NULL include reason.
pub fn count_filter_buckets(conn: &Connection)
    -> Result<(u64, u64, u64, u64)> {
    let mut stmt = conn.prepare(
        "SELECT
            SUM(CASE WHEN include_reason_archive < 0 THEN 1 ELSE 0 END) AS included,
            SUM(CASE WHEN include_reason_archive = 0 THEN 1 ELSE 0 END) AS excluded_by_include,
            SUM(CASE WHEN exclude_reason_archive > 0 AND include_reason_archive < 0
                     THEN 1 ELSE 0 END) AS excluded_by_exclude,
            SUM(CASE WHEN include_reason_archive IS NULL THEN 1 ELSE 0 END) AS unfiltered
         FROM files",
    ).to_panic()?;
    let row = stmt.query_row([], |row| {
        Ok((
            row.get::<_, i64>("included")?.max(0) as u64,
            row.get::<_, i64>("excluded_by_include")?.max(0) as u64,
            row.get::<_, i64>("excluded_by_exclude")?.max(0) as u64,
            row.get::<_, i64>("unfiltered")?.max(0) as u64,
        ))
    }).to_panic()?;
    Ok(row)
}

/// Rows with a content hash (`sha1 IS NOT NULL`).
pub fn count_hashed(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files WHERE sha1 IS NOT NULL",
        [],
        |r| r.get(0),
    ).to_panic()?;
    Ok(n as u64)
}

/// Rows carrying a given `FileFlag` bit.
pub fn count_flag(conn: &Connection, flag: FileFlag) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files WHERE (flags & :bit) != 0",
        rusqlite::named_params! { ":bit": flag.mask_i64() },
        |r| r.get(0),
    ).to_panic()?;
    Ok(n as u64)
}

/// Self-canonical payloads whose `(sha1, size)` group has exactly one member.
pub fn count_canonical_singletons(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files AS self
         WHERE self.canonical_id = self.id
           AND (SELECT COUNT(*) FROM files AS f WHERE f.canonical_id = self.canonical_id) = 1",
        [],
        |r| r.get(0),
    ).to_panic()?;
    Ok(n as u64)
}

/// Rows pointing at themselves as canonical payload.
pub fn count_canonical_files(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files WHERE canonical_id = id",
        [],
        |r| r.get(0),
    ).to_panic()?;
    Ok(n as u64)
}

/// Sum of `size` over all rows.
pub fn sum_bytes_all(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COALESCE(SUM(size), 0) FROM files",
        [],
        |r| r.get(0),
    ).to_panic()?;
    Ok(n as u64)
}

/// Sum of `size` over self-canonical payload rows.
pub fn sum_bytes_canonical(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COALESCE(SUM(size), 0) FROM files WHERE canonical_id = id",
        [],
        |r| r.get(0),
    ).to_panic()?;
    Ok(n as u64)
}

/// Sum of `size` over rows whose payload was appended to an archive session.
pub fn sum_bytes_archived(conn: &Connection) -> Result<u64> {
    let bit = FileFlag::AppendedPath.mask_i64();
    let n: i64 = conn.query_row(
        "SELECT COALESCE(SUM(size), 0) FROM files WHERE (flags & :bit) != 0",
        rusqlite::named_params! { ":bit": bit },
        |r| r.get(0),
    ).to_panic()?;
    Ok(n as u64)
}

/// Archive session bounds over **finalized** sessions:
/// `(started_at, finished_at, total_sessions)`. Timestamps are RFC3339 UTC as
/// stored; interrupted/unfinalized sessions are excluded from the bounds.
pub fn session_bounds(conn: &Connection)
    -> Result<(Option<String>, Option<String>, u64)> {
    let (started, finished, total) = conn.query_row(
        "SELECT MIN(started_at) AS started, MAX(finished_at) AS finished,
                COUNT(*) AS total
         FROM archive_sessions
         WHERE finalized = 1",
        [],
        |row| {
            Ok((
                row.get::<_, Option<String>>("started")?,
                row.get::<_, Option<String>>("finished")?,
                row.get::<_, i64>("total")? as u64,
            ))
        },
    ).to_panic()?;
    Ok((started, finished, total))
}