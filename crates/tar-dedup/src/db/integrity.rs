//! File contains db functions to check the integrity of the database

use rusqlite::{Connection, named_params};

use crate::config::{ExtractConfig, ExtractPipelinePhase, PipelinePhase};
use crate::db::common::{
    SqlFileRow, count_files_in_phase, generate_archive_and_extract_filter, generate_archive_filter,
};
use crate::db::dedup;
use crate::db::flags::{FileFlag, OutTreeFlag};
use crate::db::types::{FileId, FilePhase};
use crate::error::{Result, ToPanic};

/// For hardlink detection, grouping based on dev, inode is required. Count all files which could
/// not be treated because dev or inode is missing.
pub fn count_missing_dev_inode(conn: &Connection) -> Result<u64> {
    let res: i64 = conn.query_row(
        "SELECT COUNT(*) AS count \
        FROM files WHERE ftype = 'file' AND (inode IS NULL OR dev IS NULL)",
        [],
        |r| r.get("count")).to_panic()?;
    Ok(res as u64)
}

/// For hardlink detection, grouping based on dev, inode is required. List all files which could
/// not be treated because dev or inode is missing.
pub fn list_missing_dev_inode<R: SqlFileRow>(conn: &Connection, last_id: &FileId, batch_size: u64)
    -> Result<Vec<R>> {
    let cols = R::sql_columns(None);
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols} \
         FROM files \
         WHERE ftype = 'file' \
         AND id > :last_id
         AND (inode IS NULL \
              OR dev IS NULL)\
         ORDER BY id
         LIMIT :batch_size")).to_panic()?;
    let rows = stmt.query_map(named_params! {
            ":last_id": last_id.0,
            ":batch_size": batch_size
        }, |row| R::from_row(row, None)
    ).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

pub fn count_double_canonical_dev_inode_group(conn: &Connection) -> Result<u64> {
    let res: i64 = conn.query_row(
        "SELECT COUNT(*) AS count
        FROM (
            SELECT f.dev, f.inode
            FROM files AS f
            WHERE f.ftype = 'file'
                AND f.canonical_id IS NOT NULL
                AND f.dev IS NOT NULL
                AND f.inode IS NOT NULL
            GROUP BY f.dev, f.inode
            HAVING COUNT(DISTINCT f.canonical_id) > 1)",
        [],
        |r| r.get("count")).to_panic()?;
    Ok(res as u64)
}

pub fn list_double_canonical_dev_inode_group<R: SqlFileRow>(
    conn: &Connection, last_id: &FileId, batch_size: u64)
    -> Result<Vec<R>> {
    let cols = R::sql_columns(None);
    let mut stmt = conn.prepare(&format!("
        SELECT {cols}
        FROM files
        WHERE ftype = 'file'
            AND id > :last_id
            AND (dev, inode) IN (SELECT f.dev, f.inode
                FROM files AS f
                WHERE f.ftype = 'file'
                    AND f.canonical_id IS NOT NULL
                    AND f.dev IS NOT NULL
                    AND f.inode IS NOT NULL
                GROUP BY f.dev, f.inode
                HAVING COUNT(DISTINCT f.canonical_id) > 1))
        ORDER BY id
        LIMIT :batch_size
    ")).to_panic()?;
    let rows = stmt.query_map(
        named_params! {
            "last_id:": last_id.0,
            ":batch_size": batch_size,
        },
        |row| R::from_row(row, None)
    ).to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// Check how many rows fail the implication name -> id
pub fn count_id_implication(conn: &Connection) -> Result<u64> {
    let res: i64 = conn.query_row(
        "SELECT COUNT(*) AS count
        FROM files
        WHERE (uid IS NULL AND username IS NOT NULL)
            OR (gid IS NULL AND groupname IS NOT NULL)",
        [],
        |r| r.get("count")).to_panic()?;
    Ok(res as u64)
}

/// Check how many rows fail the implication name -> id
pub fn count_missing_unix_infos(conn: &Connection) -> Result<u64> {
    let res: i64 = conn.query_row(
        "SELECT COUNT(*) AS count
        FROM files
        WHERE uid IS NULL
            OR gid IS NULL
            OR mode IS NULL
            OR inode IS NULL
            OR dev IS NULL",
        [],
        |r| r.get("count")).to_panic()?;
    Ok(res as u64)
}

//==================================================================================================
// Cross-phase invariant battery (`plans/sanity-checks.md`)
//
// Each invariant is a dedicated `count_*` returning the number of offending rows;
// the aggregators run the subset that became established by the completed phase
// and either `assert!` (structural) or `warn` (tolerated, force/salvage cases).
//==================================================================================================

/// Config fragment the archive battery needs. The extract side runs a config-free
/// subset with a zeroed gate (`sparsify: false` so A1/A6 are skipped there).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveGate {
    pub eager_filter: bool,
    pub detect_hardlinks: bool,
    pub sparsify: bool,
    pub min_pages: u64,
}

/// A1 — hashable rows: `ErrorWhileHash ⟺ sha1 IS NULL` (sha1 is the sole fail
/// marker; `sparse_count` is defaulted to 0 and carries no signal).
fn count_hash_error_sha1_violations(conn: &Connection, eager_filter: bool,
    detect_hardlinks: bool) -> Result<u64> {
    let filtered = if eager_filter {
        format!("AND {}", generate_archive_filter(None))
    } else {
        String::new()
    };
    let hardlink = if detect_hardlinks {
        "AND (flags & :hardlink) != 0"
    } else {
        ""
    };
    let sql = format!(
        "SELECT COUNT(*) AS count FROM files
         WHERE ftype = 'file'
           {hardlink}
           {filtered}
           AND (
                ((flags & :sha_error) != 0 AND sha1 IS NOT NULL)
             OR ((flags & :sha_error) = 0 AND sha1 IS NULL)
           )"
    );
    let count: i64 = if detect_hardlinks {
        conn.query_row(&sql, named_params! {
            ":hardlink": FileFlag::FileHardlinkCanonical.mask_i64(),
            ":sha_error": FileFlag::ErrorWhileHash.mask_i64(),
        }, |r| r.get("count")).to_panic()?
    } else {
        conn.query_row(&sql, named_params! {
            ":sha_error": FileFlag::ErrorWhileHash.mask_i64(),
        }, |r| r.get("count")).to_panic()?
    };
    Ok(count as u64)
}

/// A2 — `ErrorWhileHash` AND `sha1 IS NOT NULL` (mutual exclusion).
fn count_error_while_hash_with_sha1(conn: &Connection) -> Result<u64> {
    let bit = FileFlag::ErrorWhileHash.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE (flags & :bit) != 0
           AND sha1 IS NOT NULL",
        named_params! { ":bit": bit },
        |r| r.get("count")).to_panic()?;
    Ok(count as u64)
}

/// A3 — `sha1 IS NULL ⟹ canonical_id IS NULL`.
fn count_missing_sha1_with_canonical(conn: &Connection) -> Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE sha1 IS NULL
           AND canonical_id IS NOT NULL",
        [],
        |r| r.get("count")).to_panic()?;
    Ok(count as u64)
}

/// A5 — eligible ∧ `canonical_id IS NULL` ⟹ `ErrorWhileDedup`. Eligible: file,
/// hashed, not hash-errored, passed the archive filter.
fn count_eligible_without_dedup_resolution(conn: &Connection) -> Result<u64> {
    let sha_err = FileFlag::ErrorWhileHash.mask_i64();
    let dedup_err = FileFlag::ErrorWhileDedup.mask_i64();
    let sql = format!(
        "SELECT COUNT(*) AS count FROM files
         WHERE ftype = 'file'
           AND sha1 IS NOT NULL
           AND (flags & :sha_err) = 0
           AND {}
           AND canonical_id IS NULL
           AND (flags & :dedup_err) = 0",
        generate_archive_filter(None)
    );
    let count: i64 = conn.query_row(
        &sql,
        named_params! {
            ":sha_err": sha_err,
            ":dedup_err": dedup_err,
        },
        |r| r.get("count")).to_panic()?;
    Ok(count as u64)
}

/// A6 — sparsify candidates have `HasSparse XOR ErrorWhileSparsify`.
/// Violation = the two flags agree (both or neither).
fn count_sparsify_flag_violations(conn: &Connection, min_pages: u64) -> Result<u64> {
    let has = FileFlag::HasSparse.mask_i64();
    let err = FileFlag::ErrorWhileSparsify.mask_i64();
    let sql = format!(
        "SELECT COUNT(*) AS count FROM files
         WHERE canonical_id = id
           AND ftype = 'file'
           AND sparse_count >= :min_pages
           AND {}
           AND (
                ((flags & :has) != 0 AND (flags & :err) != 0)
             OR ((flags & :has) = 0 AND (flags & :err) = 0)
           )",
        generate_archive_filter(None)
    );
    let count: i64 = conn.query_row(
        &sql,
        named_params! {
            ":has": has,
            ":err": err,
            ":min_pages": min_pages as i64,
        },
        |r| r.get("count")).to_panic()?;
    Ok(count as u64)
}

/// A7 — `canonical_id = id ⟹ AppendedPath XOR ErrorWhileArchive`. Violation =
/// the two flags agree.
fn count_append_flag_violations(conn: &Connection) -> Result<u64> {
    let app = FileFlag::AppendedPath.mask_i64();
    let err = FileFlag::ErrorWhileArchive.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE canonical_id = id
           AND (
                ((flags & :app) != 0 AND (flags & :err) != 0)
             OR ((flags & :app) = 0 AND (flags & :err) = 0)
           )",
        named_params! {
            ":app": app,
            ":err": err,
        },
        |r| r.get("count")).to_panic()?;
    Ok(count as u64)
}

/// E1 — canonical `AppendedPath ⟹ FileExtracted OR ErrorWhileExtracting`.
/// `count_missing_payloads` minus the rows tolerated via `ErrorWhileExtracting`.
fn count_e1_payload_missing(conn: &Connection) -> Result<u64> {
    let appended = FileFlag::AppendedPath.mask_i64();
    let extracted = FileFlag::FileExtracted.mask_i64();
    let extracting_err = FileFlag::ErrorWhileExtracting.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE canonical_id = id
           AND (flags & :appended) != 0
           AND (flags & :extracted) = 0
           AND (flags & :extracting_err) = 0",
        named_params! {
            ":appended": appended,
            ":extracted": extracted,
            ":extracting_err": extracting_err,
        },
        |r| r.get("count")).to_panic()?;
    Ok(count as u64)
}

/// E2 — canonical `FileExtracted OR ErrorWhileExtracting ⟹ AppendedPath`
/// (stray cache payloads / truncation salvage).
fn count_e2_stray_extracted(conn: &Connection) -> Result<u64> {
    let appended = FileFlag::AppendedPath.mask_i64();
    let extracted = FileFlag::FileExtracted.mask_i64();
    let extracting_err = FileFlag::ErrorWhileExtracting.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE canonical_id = id
           AND ((flags & :extracted) != 0 OR (flags & :extracting_err) != 0)
           AND (flags & :appended) = 0",
        named_params! {
            ":appended": appended,
            ":extracted": extracted,
            ":extracting_err": extracting_err,
        },
        |r| r.get("count")).to_panic()?;
    Ok(count as u64)
}

/// E4 — every elected rehash row ends with **exactly one** of
/// `RehashMismatch | ErrorWhileRehashing | RehashVerified`, and no rehash flag
/// appears on non-elected rows. Elected = the stable rehash workload
/// (`canonical_id=id`, file, sha1 present, `FileExtracted`, joint filter passed).
fn count_rehash_verdict_violations(conn: &Connection) -> Result<u64> {
    let mismatch = FileFlag::RehashMismatch.mask_i64();
    let err = FileFlag::ErrorWhileRehashing.mask_i64();
    let verified = FileFlag::RehashVerified.mask_i64();
    let extracted = FileFlag::FileExtracted.mask_i64();
    let elected = format!(
        "canonical_id = id AND ftype = 'file' AND sha1 IS NOT NULL \
         AND (flags & :extracted) != 0 AND {}",
        generate_archive_and_extract_filter(None)
    );
    let sql = format!(
        "SELECT COUNT(*) AS count FROM files
         WHERE (
             {elected}
             AND (
                 ((flags & :m) = 0 AND (flags & :e) = 0 AND (flags & :v) = 0)
              OR (((flags & :m) != 0) + ((flags & :e) != 0) + ((flags & :v) != 0) >= 2)
             )
         ) OR (
             NOT ({elected})
             AND ((flags & :m) != 0 OR (flags & :e) != 0 OR (flags & :v) != 0)
         )"
    );
    let count: i64 = conn.query_row(
        &sql,
        named_params! {
            ":m": mismatch,
            ":e": err,
            ":v": verified,
            ":extracted": extracted,
        },
        |r| r.get("count")).to_panic()?;
    Ok(count as u64)
}

/// E5 — every non-dir `out_tree` row's **direct** parent path exists as a dir
/// `out_tree` row (the `ensure_parent` contract). A dir row `p` is a prefix
/// parent of `o` when `o.abs_path == p.abs_path || '/' || …`; the direct parent
/// is the longest such prefix.
fn count_out_tree_missing_parent(conn: &Connection) -> Result<u64> {
    let dir = OutTreeFlag::IsDirectory.mask_i64();
    let sql = format!(
        "SELECT COUNT(*) AS count FROM out_tree o
         WHERE (o.flags & :dir) = 0
           AND NOT EXISTS (
               SELECT 1 FROM out_tree p
               WHERE (p.flags & :dir) != 0
                 AND p.abs_path != o.abs_path
                 AND substr(o.abs_path, 1, length(p.abs_path)) = p.abs_path
                 AND substr(o.abs_path, length(p.abs_path) + 1, 1) = '/'
                 AND NOT EXISTS (
                     SELECT 1 FROM out_tree p2
                     WHERE (p2.flags & :dir) != 0
                       AND p2.abs_path != o.abs_path
                       AND substr(o.abs_path, 1, length(p2.abs_path)) = p2.abs_path
                       AND substr(o.abs_path, length(p2.abs_path) + 1, 1) = '/'
                       AND length(p2.abs_path) > length(p.abs_path)
                 )
           )"
    );
    let count: i64 = conn.query_row(
        &sql,
        named_params! { ":dir": dir },
        |r| r.get("count")).to_panic()?;
    Ok(count as u64)
}

/// E6 — non-dir `out_tree` rows carry a `file_id` (`ensure_parent` injects dir
/// rows with a `NULL` file_id, never non-dir rows).
fn count_out_tree_missing_file_id(conn: &Connection) -> Result<u64> {
    let dir = OutTreeFlag::IsDirectory.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM out_tree
         WHERE (flags & :dir) = 0
           AND file_id IS NULL",
        named_params! { ":dir": dir },
        |r| r.get("count")).to_panic()?;
    Ok(count as u64)
}

/// E7 — `(flags & IsDirectory) != 0 AND file_id IS NOT NULL ⟹ referenced
/// `files.ftype = 'dir'`` (unidirectional).
fn count_out_tree_dir_mismatch(conn: &Connection) -> Result<u64> {
    let dir = OutTreeFlag::IsDirectory.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM out_tree o
         JOIN files f ON f.id = o.file_id
         WHERE (o.flags & :dir) != 0
           AND f.ftype != 'dir'",
        named_params! { ":dir": dir },
        |r| r.get("count")).to_panic()?;
    Ok(count as u64)
}

/// E8a — files: `AtLinkSource ⟹ FileExtracted` (link-tree moves the canonical).
fn count_link_source_without_extracted(conn: &Connection) -> Result<u64> {
    let at = FileFlag::AtLinkSource.mask_i64();
    let extracted = FileFlag::FileExtracted.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE (flags & :at) != 0
           AND (flags & :extracted) = 0",
        named_params! {
            ":at": at,
            ":extracted": extracted,
        },
        |r| r.get("count")).to_panic()?;
    Ok(count as u64)
}

/// E8b — files: `canonical_id = id AND (UsedRefLink OR ErrorWhilePlacing)
/// ⟹ FileExtracted`. Duplicate rows (which `apply_flags_to_files` also flags)
/// are exempt — their flag noise is their own `out_tree` outcome.
fn count_canonical_placeflag_without_extracted(conn: &Connection) -> Result<u64> {
    let reflink = FileFlag::UsedRefLink.mask_i64();
    let err = FileFlag::ErrorWhilePlacing.mask_i64();
    let extracted = FileFlag::FileExtracted.mask_i64();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM files
         WHERE canonical_id = id
           AND ((flags & :reflink) != 0 OR (flags & :err) != 0)
           AND (flags & :extracted) = 0",
        named_params! {
            ":reflink": reflink,
            ":err": err,
            ":extracted": extracted,
        },
        |r| r.get("count")).to_panic()?;
    Ok(count as u64)
}

/// E9 — `out_tree`: `ErrorWhilePlace ⟹ NOT (Placed | RemovedPrevious | Conflict
/// | IsHardlink | UsedRefLink)` — an error carries none of the trace flags.
fn count_out_tree_error_with_trace(conn: &Connection) -> Result<u64> {
    let err = OutTreeFlag::ErrorWhilePlace.mask_i64();
    let trace = OutTreeFlag::Placed.mask()
        | OutTreeFlag::RemovedPrevious.mask()
        | OutTreeFlag::Conflict.mask()
        | OutTreeFlag::IsHardlink.mask()
        | OutTreeFlag::UsedRefLink.mask();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) AS count FROM out_tree
         WHERE (flags & :err) != 0
           AND (flags & :trace) != 0",
        named_params! {
            ":err": err,
            ":trace": trace as i64,
        },
        |r| r.get("count")).to_panic()?;
    Ok(count as u64)
}

/// E10 — placed canonicals get `AppliedMetadata XOR ErrorWhileApplyingMetadata`.
/// Link-tree applies on the canonical files (`AtLinkSource`); otherwise on
/// non-dir `Placed` `canonical_id = id` `out_tree` rows; directory rows only
/// when the metadata pass touches them (`!--no-overwrite-dir`).
fn count_e10_metadata_violations(conn: &Connection, link_tree: bool, apply_dirs: bool)
    -> Result<u64> {
    if link_tree {
        let at = FileFlag::AtLinkSource.mask_i64();
        let applied = FileFlag::AppliedMetadata.mask_i64();
        let err = FileFlag::ErrorWhileApplyingMetadata.mask_i64();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) AS count FROM files
             WHERE (flags & :at) != 0
               AND (
                    ((flags & :applied) != 0 AND (flags & :err) != 0)
                 OR ((flags & :applied) = 0 AND (flags & :err) = 0)
               )",
            named_params! {
                ":at": at,
                ":applied": applied,
                ":err": err,
            },
            |r| r.get("count")).to_panic()?;
        return Ok(count as u64);
    }
    let placed = OutTreeFlag::Placed.mask_i64();
    let is_dir = OutTreeFlag::IsDirectory.mask_i64();
    let applied = OutTreeFlag::AppliedMetadata.mask_i64();
    let err = OutTreeFlag::ErrorWhileApplyingMetadata.mask_i64();
    let xor_violation = "((o.flags & :applied) != 0 AND (o.flags & :err) != 0) \
                         OR ((o.flags & :applied) = 0 AND (o.flags & :err) = 0)";
    let non_dir: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) AS count FROM out_tree o
             WHERE o.flags & :is_dir = 0
               AND o.flags & :placed != 0
               AND o.canonical_id = o.id
               AND ({xor_violation})"
        ),
        named_params! {
            ":placed": placed,
            ":is_dir": is_dir,
            ":applied": applied,
            ":err": err,
        },
        |r| r.get("count")).to_panic()?;
    let dirs: i64 = if apply_dirs {
        conn.query_row(
            &format!(
                "SELECT COUNT(*) AS count FROM out_tree o
                 WHERE o.flags & :is_dir != 0
                   AND ({xor_violation})"
            ),
            named_params! {
                ":is_dir": is_dir,
                ":applied": applied,
                ":err": err,
            },
            |r| r.get("count")).to_panic()?
    } else {
        0
    };
    Ok((non_dir + dirs) as u64)
}

/// Panic-tier check: structural invariants must be 0 after a completed phase.
fn assert_zero(name: &str, count: u64, phase: &str) {
    assert!(count == 0, "INVARIANT FAILED after `{phase}`: {name} = {count}");
}

/// Warn-tier check: tolerated cases (force / salvage semantics) get a warning.
fn warn_count(name: &str, count: u64, phase: &str) {
    if count > 0 {
        tracing::warn!("post-`{phase}` integrity note: {name} = {count}");
    }
}

/// Shared archive battery body. `include_hash` enables the hash/sparsify-scoped
/// checks (archive pipeline); the extract side passes `false` and a completed
/// `Archive` phase so only the config-free subset (A2/A3/A4/A5/A7/A8) runs.
/// `progress` fires once per completed check (advances the integrity bar).
fn check_archive_battery(conn: &Connection, gate: &ArchiveGate, phase: PipelinePhase,
    include_hash: bool, progress: Option<&dyn Fn()>) -> Result<()> {
    let label = phase.as_str();
    let done = |name: &str, count: u64| {
        assert_zero(name, count, label);
        if let Some(p) = progress {
            p()
        }
    };
    done("A2 ErrorWhileHash ∧ sha1 NOT NULL", count_error_while_hash_with_sha1(conn)?);
    done("A3 sha1 IS NULL ⟹ canonical_id IS NULL", count_missing_sha1_with_canonical(conn)?);
    done("A4 CheckWithCanonicalCompleted", dedup::count_check_with_canonical_completed(conn)?);
    // A1 is established by the hash phase. Gate on the phase set (not the index):
    // in eager mode `filter` — index 2 — precedes `hash`, so an index comparison
    // would flag the not-yet-hashed rows.
    if include_hash && matches!(phase,
        PipelinePhase::Hash | PipelinePhase::Dedup | PipelinePhase::Sparsify
        | PipelinePhase::Stage | PipelinePhase::Archive) {
        done("A1 hashable ErrorWhileHash ⟺ sha1 IS NULL",
             count_hash_error_sha1_violations(conn, gate.eager_filter, gate.detect_hardlinks)?);
    }
    if phase.index() >= PipelinePhase::Dedup.index() {
        done("A5 eligible ∧ canonical NULL ⟹ ErrorWhileDedup",
             count_eligible_without_dedup_resolution(conn)?);
    }
    if include_hash && gate.sparsify && phase.index() >= PipelinePhase::Sparsify.index() {
        done("A6 sparsify HasSparse XOR ErrorWhileSparsify",
             count_sparsify_flag_violations(conn, gate.min_pages)?);
    }
    if phase.index() >= PipelinePhase::Archive.index() {
        done("A7 canonical ⟹ AppendedPath XOR ErrorWhileArchive",
             count_append_flag_violations(conn)?);
        done("A8 count_in_phase(Staged)", count_files_in_phase(conn, FilePhase::Staged)?);
    }
    Ok(())
}

/// Number of checks [`check_after_archive_phase`] runs for `phase` (under the
/// same gating) — the integrity bar's length. Callers size the bar with this to
/// keep `{pos}/{len}` truthful.
pub fn archive_phase_check_count(gate: &ArchiveGate, phase: PipelinePhase) -> u64 {
    archive_battery_count(gate, phase, true)
}

fn archive_battery_count(gate: &ArchiveGate, phase: PipelinePhase, include_hash: bool) -> u64 {
    let mut n = 3; // A2, A3, A4
    if include_hash && matches!(phase,
        PipelinePhase::Hash | PipelinePhase::Dedup | PipelinePhase::Sparsify
        | PipelinePhase::Stage | PipelinePhase::Archive) {
        n += 1; // A1
    }
    if phase.index() >= PipelinePhase::Dedup.index() {
        n += 1; // A5
    }
    if include_hash && gate.sparsify && phase.index() >= PipelinePhase::Sparsify.index() {
        n += 1; // A6
    }
    if phase.index() >= PipelinePhase::Archive.index() {
        n += 2; // A7, A8
    }
    n
}

/// Run the archive battery after `phase` completed successfully. Always-on
/// (not gated behind debug assertions). See `plans/sanity-checks.md` A1–A8.
pub fn check_after_archive_phase(conn: &Connection, gate: &ArchiveGate, phase: PipelinePhase,
    progress: Option<&dyn Fn()>) -> Result<()> {
    check_archive_battery(conn, gate, phase, true, progress)
}

/// Number of checks [`check_after_extract_phase`] runs (config-free archive
/// subset + extract battery) — the integrity bar's length.
pub fn extract_phase_check_count(config: &ExtractConfig, phase: ExtractPipelinePhase) -> u64 {
    let config_free = ArchiveGate {
        eager_filter: false, detect_hardlinks: false, sparsify: false, min_pages: 0,
    };
    archive_battery_count(&config_free, PipelinePhase::Archive, false)
        + extract_battery_count(config, phase)
}

fn extract_battery_count(config: &ExtractConfig, phase: ExtractPipelinePhase) -> u64 {
    let mut n = 3; // E1, E2, E3
    if phase.index() >= ExtractPipelinePhase::Rehash.index() && config.rehash {
        n += 1; // E4
    }
    if phase.index() >= ExtractPipelinePhase::PlacementPrologue.index() {
        n += 3; // E5, E6, E7
    }
    if phase.index() >= ExtractPipelinePhase::Place.index() {
        n += 3; // E8a, E8b, E9
    }
    if phase.index() >= ExtractPipelinePhase::Permissions.index() {
        n += 1; // E10
    }
    n
}

/// Run the archive battery (config-free subset, the archive already completed on
/// the producing side) followed by the extract battery, after `phase` finished.
/// See `plans/sanity-checks.md` E1–E10.
pub fn check_after_extract_phase(conn: &Connection, config: &ExtractConfig,
    phase: ExtractPipelinePhase, progress: Option<&dyn Fn()>) -> Result<()> {
    let gate = ArchiveGate {
        eager_filter: false,
        detect_hardlinks: false,
        sparsify: false,
        min_pages: 0,
    };
    // The archive phase already completed; its config-free invariants must hold.
    check_archive_battery(conn, &gate, PipelinePhase::Archive, false, progress)?;

    let label = phase.as_str();
    let note = |name: &str, count: u64| {
        warn_count(name, count, label);
        if let Some(p) = progress {
            p()
        }
    };
    let done = |name: &str, count: u64| {
        assert_zero(name, count, label);
        if let Some(p) = progress {
            p()
        }
    };
    note("E1 AppendedPath ⟹ FileExtracted OR ErrorWhileExtracting",
         count_e1_payload_missing(conn)?);
    note("E2 FileExtracted OR ErrorWhileExtracting ⟹ AppendedPath",
         count_e2_stray_extracted(conn)?);
    done("E3 count_in_phase(Archived)", count_files_in_phase(conn, FilePhase::Archived)?);
    if phase.index() >= ExtractPipelinePhase::Rehash.index() && config.rehash {
        done("E4 rehash verdict exactly-one / none on non-elected",
             count_rehash_verdict_violations(conn)?);
    }
    if phase.index() >= ExtractPipelinePhase::PlacementPrologue.index() {
        done("E5 non-dir out_tree parent exists as dir row",
             count_out_tree_missing_parent(conn)?);
        done("E6 non-dir out_tree file_id NOT NULL", count_out_tree_missing_file_id(conn)?);
        done("E7 dir-flag ∩ file_id ⟹ files.ftype='dir'", count_out_tree_dir_mismatch(conn)?);
    }
    if phase.index() >= ExtractPipelinePhase::Place.index() {
        done("E8a AtLinkSource ⟹ FileExtracted", count_link_source_without_extracted(conn)?);
        done("E8b canonical ∧ (UsedRefLink | ErrorWhilePlacing) ⟹ FileExtracted",
             count_canonical_placeflag_without_extracted(conn)?);
        done("E9 ErrorWhilePlace ⟹ no placement trace flags",
             count_out_tree_error_with_trace(conn)?);
    }
    if phase.index() >= ExtractPipelinePhase::Permissions.index() {
        let apply_dirs = !config.attributes.no_overwrite_dir;
        done("E10 AppliedMetadata XOR ErrorWhileApplyingMetadata",
             count_e10_metadata_violations(conn, config.placement.link_tree, apply_dirs)?);
    }
    Ok(())
}

//==================================================================================================
// Testing — one count_* per invariant: clean DB → 0, single violation → 1, and the
// aggregators panic on a violation (`catch_unwind`). Real `schema::initialize`.
//==================================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::flags::FlagBit;
    use crate::db::schema;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::PathBuf;

    fn open_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = Connection::open(&dir.path().join("t.sqlite")).expect("open conn");
        schema::initialize(&conn).expect("schema init");
        // Internal include (id -1) in both tables + an id-5 exclude to break the
        // archive/extract filters without tripping the FK.
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

    /// Insert a `files` row passing both filters. `flags` is a raw OR of `FileFlag` masks.
    fn file_row(
        conn: &Connection, id: i64, abs_path: &str, ftype: &str, phase: &str,
        sha1: Option<&[u8; 20]>, canonical: Option<i64>, sparse: i64, flags: i64) {
        conn.execute(
            "INSERT INTO files (id, abs_path, abs_path_raw, ext, size, ftype, phase, sha1, \
             include_reason_archive, exclude_reason_archive, \
             include_reason_extract, exclude_reason_extract, \
             canonical_id, sparse_count, flags) \
             VALUES (:id, :abs_path, :abs_path_raw, '.bin', 0, :ftype, :phase, :sha1, \
                     -1, 0, -1, 0, :canon, :sparse, :flags)",
            named_params! {
                ":id": id,
                ":abs_path": abs_path,
                ":abs_path_raw": abs_path.as_bytes().to_vec(),
                ":ftype": ftype,
                ":phase": phase,
                ":sha1": sha1.map(|s| s.as_slice()),
                ":canon": canonical,
                ":sparse": sparse,
                ":flags": flags,
            },
        ).expect("insert file row");
    }

    fn out_tree_row(conn: &Connection, id: i64, abs_path: &str, file_id: Option<i64>,
        canonical: Option<i64>, flags: i64) {
        conn.execute(
            "INSERT INTO out_tree (id, abs_path, file_id, canonical_id, flags) \
             VALUES (:id, :abs_path, :file_id, :canonical, :flags)",
            named_params! {
                ":id": id,
                ":abs_path": abs_path,
                ":file_id": file_id,
                ":canonical": canonical,
                ":flags": flags,
            },
        ).expect("insert out_tree row");
    }

    fn m(flag: impl FlagBit) -> i64 { flag.mask_i64() }

    #[test]
    fn a1_hash_error_implication() {
        let (_dir, conn) = open_db();
        // clean: hashed file, no error
        file_row(&conn, 1, "/f1", "file", "hashed", Some(&[1; 20]), Some(1), 0, 0);
        assert_eq!(count_hash_error_sha1_violations(&conn, true, false).unwrap(), 0);

        // not hashed, no error -> violation (⟺ broken both ways)
        file_row(&conn, 2, "/f2", "file", "inventoried", None, None, 0, 0);
        assert_eq!(count_hash_error_sha1_violations(&conn, true, false).unwrap(), 1);

        // error flag AND sha1 present -> violation
        file_row(&conn, 3, "/f3", "file", "hashed", Some(&[3; 20]), Some(3), 0,
                 m(FileFlag::ErrorWhileHash));
        assert_eq!(count_hash_error_sha1_violations(&conn, true, false).unwrap(), 2);

        // eager filter excludes the unhashed row -> back to 1
        conn.execute("UPDATE files SET exclude_reason_archive = 5 WHERE id = 2", [])
            .expect("exclude");
        assert_eq!(count_hash_error_sha1_violations(&conn, true, false).unwrap(), 1);
        // lazy scope ignores the filter -> 2 again
        assert_eq!(count_hash_error_sha1_violations(&conn, false, false).unwrap(), 2);
    }

    #[test]
    fn a2_error_while_hash_with_sha1() {
        let (_dir, conn) = open_db();
        file_row(&conn, 1, "/f1", "file", "hashed", Some(&[1; 20]), Some(1), 0, 0);
        file_row(&conn, 2, "/f2", "file", "inventoried", None, None, 0,
                 m(FileFlag::ErrorWhileHash));
        assert_eq!(count_error_while_hash_with_sha1(&conn).unwrap(), 0);

        file_row(&conn, 3, "/f3", "file", "hashed", Some(&[3; 20]), Some(3), 0,
                 m(FileFlag::ErrorWhileHash));
        assert_eq!(count_error_while_hash_with_sha1(&conn).unwrap(), 1);
    }

    #[test]
    fn a3_missing_sha1_with_canonical() {
        let (_dir, conn) = open_db();
        file_row(&conn, 1, "/f1", "file", "hashed", Some(&[1; 20]), Some(1), 0, 0);
        file_row(&conn, 2, "/f2", "file", "inventoried", None, None, 0, 0);
        assert_eq!(count_missing_sha1_with_canonical(&conn).unwrap(), 0);

        file_row(&conn, 3, "/f3", "file", "inventoried", None, Some(3), 0, 0);
        assert_eq!(count_missing_sha1_with_canonical(&conn).unwrap(), 1);
    }

    #[test]
    fn a5_eligible_without_dedup_resolution() {
        let (_dir, conn) = open_db();
        file_row(&conn, 1, "/f1", "file", "deduped", Some(&[1; 20]), Some(1), 0, 0);
        file_row(&conn, 2, "/f2", "file", "deduped", Some(&[1; 20]), Some(1), 0,
                 m(FileFlag::ErrorWhileDedup));
        file_row(&conn, 3, "/f3", "dir", "deduped", None, None, 0, 0);
        assert_eq!(count_eligible_without_dedup_resolution(&conn).unwrap(), 0);

        file_row(&conn, 4, "/f4", "file", "deduped", Some(&[4; 20]), None, 0, 0);
        assert_eq!(count_eligible_without_dedup_resolution(&conn).unwrap(), 1);
    }

    #[test]
    fn a6_sparsify_xor() {
        let (_dir, conn) = open_db();
        file_row(&conn, 1, "/f1", "file", "sparsified", Some(&[1; 20]), Some(1), 8,
                 m(FileFlag::HasSparse));
        file_row(&conn, 2, "/f2", "file", "sparsified", Some(&[2; 20]), Some(2), 8,
                 m(FileFlag::ErrorWhileSparsify));
        file_row(&conn, 3, "/f3", "file", "sparsified", Some(&[3; 20]), Some(3), 1,
                 m(FileFlag::HasSparse));
        assert_eq!(count_sparsify_flag_violations(&conn, 4).unwrap(), 0);

        // candidate with neither flag -> violation
        file_row(&conn, 4, "/f4", "file", "deduped", Some(&[4; 20]), Some(4), 8, 0);
        assert_eq!(count_sparsify_flag_violations(&conn, 4).unwrap(), 1);
        // candidate with both -> violation
        file_row(&conn, 5, "/f5", "file", "sparsified", Some(&[5; 20]), Some(5), 8,
                 m(FileFlag::HasSparse) | m(FileFlag::ErrorWhileSparsify));
        assert_eq!(count_sparsify_flag_violations(&conn, 4).unwrap(), 2);
    }

    #[test]
    fn a7_append_xor() {
        let (_dir, conn) = open_db();
        file_row(&conn, 1, "/f1", "file", "archived", Some(&[1; 20]), Some(1), 0,
                 m(FileFlag::AppendedPath));
        file_row(&conn, 2, "/f2", "file", "archived", Some(&[2; 20]), Some(2), 0,
                 m(FileFlag::ErrorWhileArchive));
        assert_eq!(count_append_flag_violations(&conn).unwrap(), 0);

        // neither flag
        file_row(&conn, 3, "/f3", "file", "archived", Some(&[3; 20]), Some(3), 0, 0);
        assert_eq!(count_append_flag_violations(&conn).unwrap(), 1);
        // both
        file_row(&conn, 4, "/f4", "file", "archived", Some(&[4; 20]), Some(4), 0,
                 m(FileFlag::AppendedPath) | m(FileFlag::ErrorWhileArchive));
        assert_eq!(count_append_flag_violations(&conn).unwrap(), 2);
        // duplicate (canonical != id) with an append flag is out of scope
        file_row(&conn, 5, "/f5", "file", "archived", Some(&[5; 20]), Some(1), 0,
                 m(FileFlag::AppendedPath));
        assert_eq!(count_append_flag_violations(&conn).unwrap(), 2);
    }

    #[test]
    fn e1_payload_missing() {
        let (_dir, conn) = open_db();
        file_row(&conn, 1, "/f1", "file", "unarchived", Some(&[1; 20]), Some(1), 0,
                 m(FileFlag::AppendedPath) | m(FileFlag::FileExtracted));
        file_row(&conn, 2, "/f2", "file", "unarchived", Some(&[2; 20]), Some(2), 0,
                 m(FileFlag::AppendedPath) | m(FileFlag::ErrorWhileExtracting));
        assert_eq!(count_e1_payload_missing(&conn).unwrap(), 0);

        // missing payload, not tolerated
        file_row(&conn, 3, "/f3", "file", "archived", Some(&[3; 20]), Some(3), 0,
                 m(FileFlag::AppendedPath));
        assert_eq!(count_e1_payload_missing(&conn).unwrap(), 1);
        // duplicate with AppendedPath is out of scope
        file_row(&conn, 4, "/f4", "file", "archived", Some(&[4; 20]), Some(1), 0,
                 m(FileFlag::AppendedPath));
        assert_eq!(count_e1_payload_missing(&conn).unwrap(), 1);
    }

    #[test]
    fn e2_stray_extracted() {
        let (_dir, conn) = open_db();
        file_row(&conn, 1, "/f1", "file", "unarchived", Some(&[1; 20]), Some(1), 0,
                 m(FileFlag::AppendedPath) | m(FileFlag::FileExtracted));
        assert_eq!(count_e2_stray_extracted(&conn).unwrap(), 0);

        file_row(&conn, 2, "/f2", "file", "unarchived", Some(&[2; 20]), Some(2), 0,
                 m(FileFlag::FileExtracted));
        assert_eq!(count_e2_stray_extracted(&conn).unwrap(), 1);
        file_row(&conn, 3, "/f3", "file", "unarchived", Some(&[3; 20]), Some(3), 0,
                 m(FileFlag::ErrorWhileExtracting));
        assert_eq!(count_e2_stray_extracted(&conn).unwrap(), 2);
    }

    #[test]
    fn e4_rehash_verdict() {
        let (_dir, conn) = open_db();
        let elected = |id: i64| {
            file_row(&conn, id, &format!("/f{id}"), "file", "rehashed",
                     Some(&[id as u8; 20]), Some(id), 0, m(FileFlag::FileExtracted))
        };
        // clean: exactly one verdict on each elected row
        elected(1);
        conn.execute("UPDATE files SET flags = flags | :v WHERE id = 1",
                     named_params! { ":v": m(FileFlag::RehashVerified) }).unwrap();
        elected(2);
        conn.execute("UPDATE files SET flags = flags | :v WHERE id = 2",
                     named_params! { ":v": m(FileFlag::RehashMismatch) }).unwrap();
        elected(3);
        conn.execute("UPDATE files SET flags = flags | :v WHERE id = 3",
                     named_params! { ":v": m(FileFlag::ErrorWhileRehashing) }).unwrap();
        assert_eq!(count_rehash_verdict_violations(&conn).unwrap(), 0);

        // elected row with no verdict
        elected(4);
        assert_eq!(count_rehash_verdict_violations(&conn).unwrap(), 1);
        // elected row with two verdicts
        elected(5);
        conn.execute("UPDATE files SET flags = flags | :a | :b WHERE id = 5",
                     named_params! {
                         ":a": m(FileFlag::RehashMismatch),
                         ":b": m(FileFlag::ErrorWhileRehashing),
                     }).unwrap();
        assert_eq!(count_rehash_verdict_violations(&conn).unwrap(), 2);
        // non-elected row carrying a verdict (duplicate, canonical != id)
        file_row(&conn, 6, "/f6", "file", "rehashed", Some(&[6; 20]), Some(1), 0,
                 m(FileFlag::FileExtracted) | m(FileFlag::RehashVerified));
        assert_eq!(count_rehash_verdict_violations(&conn).unwrap(), 3);
        // exhausted test cleanup deleted by tempdir
    }

    #[test]
    fn e5_out_tree_parent() {
        let (_dir, conn) = open_db();
        out_tree_row(&conn, 1, "/ed/a/b", None, None, m(OutTreeFlag::IsDirectory));
        out_tree_row(&conn, 2, "/ed/a/b/child.txt", None, None, 0);
        assert_eq!(count_out_tree_missing_parent(&conn).unwrap(), 0);

        out_tree_row(&conn, 3, "/ed/x/y.txt", None, None, 0);
        assert_eq!(count_out_tree_missing_parent(&conn).unwrap(), 1);
        // backing dir now exists -> resolved
        out_tree_row(&conn, 4, "/ed/x", None, None, m(OutTreeFlag::IsDirectory));
        assert_eq!(count_out_tree_missing_parent(&conn).unwrap(), 0);
    }

    #[test]
    fn e6_out_tree_missing_file_id() {
        let (_dir, conn) = open_db();
        file_row(&conn, 1, "/f1", "file", "unarchived", Some(&[1; 20]), Some(1), 0, 0);
        out_tree_row(&conn, 1, "/ed/a", None, None, m(OutTreeFlag::IsDirectory));
        out_tree_row(&conn, 2, "/ed/a/child.txt", Some(1), Some(1), 0);
        assert_eq!(count_out_tree_missing_file_id(&conn).unwrap(), 0);

        out_tree_row(&conn, 3, "/ed/a/broken.txt", None, None, 0);
        assert_eq!(count_out_tree_missing_file_id(&conn).unwrap(), 1);
    }

    #[test]
    fn e7_out_tree_dir_mismatch() {
        let (_dir, conn) = open_db();
        file_row(&conn, 1, "/dir1", "dir", "unarchived", None, None, 0, 0);
        out_tree_row(&conn, 1, "/ed/dir1", Some(1), None, m(OutTreeFlag::IsDirectory));
        assert_eq!(count_out_tree_dir_mismatch(&conn).unwrap(), 0);

        file_row(&conn, 2, "/file2", "file", "unarchived", Some(&[2; 20]), Some(2), 0, 0);
        out_tree_row(&conn, 2, "/ed/file2", Some(2), None, m(OutTreeFlag::IsDirectory));
        assert_eq!(count_out_tree_dir_mismatch(&conn).unwrap(), 1);
    }

    #[test]
    fn e8a_link_source_without_extracted() {
        let (_dir, conn) = open_db();
        file_row(&conn, 1, "/f1", "file", "unarchived", Some(&[1; 20]), Some(1), 0,
                 m(FileFlag::AtLinkSource) | m(FileFlag::FileExtracted));
        file_row(&conn, 2, "/f2", "file", "unarchived", Some(&[2; 20]), Some(2), 0,
                 m(FileFlag::AtLinkSource));
        assert_eq!(count_link_source_without_extracted(&conn).unwrap(), 1);
    }

    #[test]
    fn e8b_canonical_placeflag_without_extracted() {
        let (_dir, conn) = open_db();
        file_row(&conn, 1, "/f1", "file", "unarchived", Some(&[1; 20]), Some(1), 0,
                 m(FileFlag::UsedRefLink) | m(FileFlag::FileExtracted));
        // duplicate carrying the place flags is exempt (its own out_tree outcome)
        file_row(&conn, 2, "/f2", "file", "unarchived", Some(&[1; 20]), Some(1), 0,
                 m(FileFlag::UsedRefLink));
        assert_eq!(count_canonical_placeflag_without_extracted(&conn).unwrap(), 0);

        file_row(&conn, 3, "/f3", "file", "unarchived", Some(&[3; 20]), Some(3), 0,
                 m(FileFlag::ErrorWhilePlacing));
        assert_eq!(count_canonical_placeflag_without_extracted(&conn).unwrap(), 1);
    }

    #[test]
    fn e9_out_tree_error_with_trace() {
        let (_dir, conn) = open_db();
        out_tree_row(&conn, 1, "/ed/a/broken.txt", None, None,
                     m(OutTreeFlag::ErrorWhilePlace));
        assert_eq!(count_out_tree_error_with_trace(&conn).unwrap(), 0);

        out_tree_row(&conn, 2, "/ed/a/ok-ish.txt", None, None,
                     m(OutTreeFlag::ErrorWhilePlace) | m(OutTreeFlag::Placed));
        out_tree_row(&conn, 3, "/ed/a/conflicted.txt", None, None,
                     m(OutTreeFlag::ErrorWhilePlace) | m(OutTreeFlag::Conflict));
        assert_eq!(count_out_tree_error_with_trace(&conn).unwrap(), 2);
    }

    #[test]
    fn e10_metadata_xor() {
        let (_dir, conn) = open_db();
        // non-link-tree out_tree scope
        let placed = m(OutTreeFlag::Placed);
        let done = m(OutTreeFlag::AppliedMetadata);
        let err = m(OutTreeFlag::ErrorWhileApplyingMetadata);
        out_tree_row(&conn, 1, "/ed/a/f1.txt", None, Some(1), placed | done);
        out_tree_row(&conn, 2, "/ed/a/f2.txt", None, Some(2), placed | err);
        out_tree_row(&conn, 3, "/ed/a/f3.txt", None, Some(3), placed);
        assert_eq!(count_e10_metadata_violations(&conn, false, false).unwrap(), 1);

        // dir scope applies only when the metadata pass touches dirs
        out_tree_row(&conn, 4, "/ed/ad", None, None, m(OutTreeFlag::IsDirectory));
        assert_eq!(count_e10_metadata_violations(&conn, false, true).unwrap(), 2);
        assert_eq!(count_e10_metadata_violations(&conn, false, false).unwrap(), 1);

        // link-tree scope on the files table
        let (_dir2, conn2) = open_db();
        file_row(&conn2, 1, "/f1", "file", "unarchived", Some(&[1; 20]), Some(1), 0,
                 m(FileFlag::AtLinkSource) | m(FileFlag::AppliedMetadata));
        file_row(&conn2, 2, "/f2", "file", "unarchived", Some(&[2; 20]), Some(2), 0,
                 m(FileFlag::AtLinkSource));
        assert_eq!(count_e10_metadata_violations(&conn2, true, false).unwrap(), 1);
    }

    #[test]
    fn archive_battery_clean_passes_and_violation_panics() {
        let gate = ArchiveGate {
            eager_filter: true, detect_hardlinks: false, sparsify: true, min_pages: 4,
        };
        // clean DB passes at every completed phase
        let (_dir, conn) = open_db();
        for phase in [
            PipelinePhase::Inventory, PipelinePhase::Hash, PipelinePhase::Filter,
            PipelinePhase::Dedup, PipelinePhase::Sparsify, PipelinePhase::Stage,
            PipelinePhase::Archive,
        ] {
            check_after_archive_phase(&conn, &gate, phase, None).expect("clean archive phase");
        }

        // a canonical row missing both append flags trips A7 after the archive phase
        let (_dir2, conn2) = open_db();
        file_row(&conn2, 1, "/f1", "file", "deduped", Some(&[1; 20]), Some(1), 0, 0);
        let res = catch_unwind(AssertUnwindSafe(|| {
            check_after_archive_phase(&conn2, &gate, PipelinePhase::Archive, None)
        }));
        assert!(res.is_err(), "A7 must panic on unappended canonicals");
    }

    #[test]
    fn progress_ticks_match_phase_check_count() {
        // The progress callback fires once per completed check; its total must
        // match the count function that sizes the integrity bar.
        let gate = ArchiveGate {
            eager_filter: true, detect_hardlinks: false, sparsify: true, min_pages: 4,
        };
        for phase in [
            PipelinePhase::Inventory, PipelinePhase::Hash, PipelinePhase::Filter,
            PipelinePhase::Dedup, PipelinePhase::Sparsify, PipelinePhase::Stage,
            PipelinePhase::Archive,
        ] {
            let (_dir, conn) = open_db();
            let ticks = std::cell::Cell::new(0u64);
            let tick = || ticks.set(ticks.get() + 1);
            check_after_archive_phase(&conn, &gate, phase, Some(&tick)).expect("clean");
            assert_eq!(ticks.get(), archive_phase_check_count(&gate, phase),
                       "archive ticks for {phase:?}");
        }

        let config = crate::config::ExtractConfig::for_place_test(
            PathBuf::from("/tmp/out"), false, 1,
        );
        for phase in [
            ExtractPipelinePhase::ScanTar, ExtractPipelinePhase::Filter,
            ExtractPipelinePhase::Rehash, ExtractPipelinePhase::PlacementPrologue,
            ExtractPipelinePhase::Place, ExtractPipelinePhase::Permissions,
        ] {
            let (_dir, conn) = open_db();
            let ticks = std::cell::Cell::new(0u64);
            let tick = || ticks.set(ticks.get() + 1);
            check_after_extract_phase(&conn, &config, phase, Some(&tick)).expect("clean");
            assert_eq!(ticks.get(), extract_phase_check_count(&config, phase),
                       "extract ticks for {phase:?}");
        }
    }

    #[test]
    fn extract_battery_clean_passes_and_violation_panics() {
        let config = crate::config::ExtractConfig::for_place_test(
            PathBuf::from("/tmp/out"), false, 1,
        );
        // clean DB passes at every phase
        let (_dir, conn) = open_db();
        for phase in [
            ExtractPipelinePhase::ScanTar, ExtractPipelinePhase::Filter,
            ExtractPipelinePhase::Rehash, ExtractPipelinePhase::PlacementPrologue,
            ExtractPipelinePhase::Place, ExtractPipelinePhase::Permissions,
        ] {
            check_after_extract_phase(&conn, &config, phase, None).expect("clean extract phase");
        }

        // an elected row without a rehash verdict trips E4 (archive battery still passes)
        let (_dir2, conn2) = open_db();
        file_row(&conn2, 1, "/f1", "file", "rehashed", Some(&[1; 20]), Some(1), 0,
                 m(FileFlag::FileExtracted) | m(FileFlag::AppendedPath));
        assert!(count_rehash_verdict_violations(&conn2).unwrap() == 1);
        let res = catch_unwind(AssertUnwindSafe(|| {
            check_after_extract_phase(&conn2, &config, ExtractPipelinePhase::Rehash, None)
        }));
        assert!(res.is_err(), "E4 must panic on a missing rehash verdict");
    }
}
