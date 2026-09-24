use chrono::{DateTime, Utc};
use rusqlite::{Connection, named_params};
use std::path::PathBuf;

use crate::db::common::{SqlFileRow, generate_archive_filter};
use crate::db::flags::{FileFlag, set_file_flag};
use crate::db::types::{FileId, FilePhase, StrippedRecord};
use crate::error::{FileStatError, Result, ToPanic};

/// One finished compare: both keys always present.
/// `Ok(equal)` on a completed byte compare; `Err((file_id, error))` for the side
/// that failed IO, carrying the downcast [`FileStatError`] for the error log.
pub struct CompareOutcome {
    pub canonical_id: FileId,
    pub candidate_id: FileId,
    pub equal: std::result::Result<bool, (FileId, FileStatError)>,
    pub canonical_modified: bool,
    pub candidate_modified: bool,
}

#[derive(Clone)]
pub struct ComparePair {
    pub canonical_id: FileId,
    pub canonical_path: PathBuf,
    pub canonical_mtime: Option<DateTime<Utc>>,
    pub canonical_atime: Option<DateTime<Utc>>,
    pub canonical_ctime: Option<DateTime<Utc>>,
    pub canonical_device_id: Option<u64>,
    pub canonical_inode_id: Option<u64>,

    pub candidate_id: FileId,
    pub candidate_path: PathBuf,
    pub candidate_size: u64,
    pub candidate_mtime: Option<DateTime<Utc>>,
    pub candidate_atime: Option<DateTime<Utc>>,
    pub candidate_ctime: Option<DateTime<Utc>>,
    pub candidate_device_id: Option<u64>,
    pub candidate_inode_id: Option<u64>,
}

/// Build ComparePair struct from a candidate + active canonical.
pub fn compare_pair(canonical: &StrippedRecord, candidate: &StrippedRecord) -> ComparePair {
    ComparePair {
        canonical_id: canonical.id,
        canonical_path: canonical.abs_path.to_path_buf(),
        canonical_mtime: canonical.mtime,
        canonical_atime: canonical.atime,
        canonical_ctime: canonical.ctime,
        canonical_device_id: canonical.device_id,
        canonical_inode_id: canonical.inode_id,

        candidate_id: candidate.id,
        candidate_path: candidate.abs_path.to_path_buf(),
        candidate_size: candidate.size,
        candidate_mtime: candidate.mtime,
        candidate_atime: candidate.atime,
        candidate_ctime: candidate.ctime,
        candidate_device_id: candidate.device_id,
        candidate_inode_id: candidate.inode_id,
    }
}

fn prev_phase(eager_filter: bool) -> &'static str {
    if eager_filter {
        FilePhase::Hashed.as_str()
    } else {
        FilePhase::Filtered.as_str()
    }
}

// INFO: Dedup group state machine
//
// `dedup_progress (sha1, size) -> state` tracks each duplicate-content group:
//
//     ready -> searching -> finished -> {errored | done | ready -> ...}
//
// All transitions are guarded SQL (they only fire for groups whose member set
// satisfies the condition), so the per-group FSM advances independently: fast
// groups churn through rounds while a slow 50 GiB group's compare is still in
// its first round. Group preconditions only look at `phase = <prev>` rows, so
// members promoted to `deduped` leave the in-round set.
//
// `dedup_inflight` is a per-connection TEMPORARY table keeping the candidate
// re-scan exactly-once (see `list_pending_comparisons`). TEMP tables are gone
// with the connection, so a crash/resume starts with an empty marker set and
// re-lists candidates whose compare was never applied.

/// Create the `dedup_progress` group table plus the `dedup_inflight` marker
/// table (a real SQLite TEMPORARY table — cleared every connection, no drop).
/// Idempotent.
pub fn create_temp_dedup_table(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS dedup_progress (
             sha1  BLOB    NOT NULL,
             size  INTEGER NOT NULL CHECK(size > 0),
             state TEXT NOT NULL CHECK(state IN
                 ('ready', 'searching', 'finished', 'errored', 'done'))
                 DEFAULT 'ready',
             PRIMARY KEY (sha1, size)
         )",
        [],
    ).to_panic()?;
    conn.execute(
        "CREATE TEMP TABLE IF NOT EXISTS dedup_inflight (
             candidate_id INTEGER PRIMARY KEY
         )",
        [],
    ).to_panic()?;
    Ok(())
}

/// Drop the `dedup_progress` group table. Idempotent. Call on the phase-success
/// path only; it must survive interrupts so a resumed run reuses group state.
pub fn drop_temp_dedup_table(conn: &Connection) -> Result<()> {
    conn.execute("DROP TABLE IF EXISTS dedup_progress", []).to_panic()?;
    conn.execute("DROP TABLE IF EXISTS dedup_inflight", []).to_panic()?;
    Ok(())
}

/// Add duplicate groups into `dedup_progress`. Idempotent: `INSERT OR IGNORE`
/// (via the `(sha1, size)` PK) keeps groups from an earlier populate and adds
/// only fresh ones, so the set is invariant to resuming mid-phase.
pub fn populate_temp_table(conn: &Connection, eager_filter: bool) -> Result<u64> {
    let n = conn.execute(
        &format!(
            "INSERT OR IGNORE INTO dedup_progress (sha1, size)
             SELECT sha1, size
                FROM files
                WHERE sha1 IS NOT NULL AND phase = '{}'
                GROUP BY sha1, size
                HAVING COUNT(*) > 1",
            prev_phase(eager_filter)
        ),
        [],
    ).to_panic()?;
    Ok(n as u64)
}

/// Groups that still need work: not yet terminal.
pub fn count_pending_dedup_groups(conn: &Connection) -> Result<u64> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM dedup_progress WHERE state NOT IN ('done', 'errored')",
        [],
        |row| row.get(0),
    ).to_panic()?;
    Ok(n as u64)
}

/// Files inside duplicate `(sha1, size)` groups (the dedup workload), whether
/// still in `<prev>` or already promoted. Used as the phase-bar total: every
/// one of these ends the phase `deduped`.
pub fn count_dedup_phase_total(conn: &Connection, eager_filter: bool) -> Result<u64> {
    let sql = format!(
        "SELECT COUNT(*) FROM files WHERE {}",
        dedup_workload_where(eager_filter)
    );
    let n: i64 = conn.query_row(&sql, [], |row| row.get(0)).to_panic()?;
    Ok(n as u64)
}

/// Of [`count_dedup_phase_total`], how many are already `deduped` — the check
/// position on a resume.
pub fn count_dedup_phase_position(conn: &Connection, eager_filter: bool) -> Result<u64> {
    let phase = prev_phase(eager_filter);
    let sql = "
        SELECT COUNT(*) FROM files
         WHERE phase = 'deduped'
           AND (sha1, size) IN (
               SELECT sha1, size FROM files WHERE sha1 IS NOT NULL
               GROUP BY sha1, size HAVING COUNT(*) > 1
           )";
    // `dedup_workload_where` alone would be wrong only in the eager/non-eager
    // sense; here the `deduped` literal makes `phase` unused. Keep the explicit
    // member test so the workload stays stable across phase boundaries.
    let _ = phase;
    let n: i64 = conn.query_row(&sql, [], |row| row.get(0)).to_panic()?;
    Ok(n as u64)
}

fn dedup_workload_where(eager_filter: bool) -> String {
    let phase = prev_phase(eager_filter);
    format!(
        "phase IN ('{phase}', 'deduped')
         AND (sha1, size) IN (
             SELECT sha1, size FROM files WHERE sha1 IS NOT NULL
             GROUP BY sha1, size HAVING COUNT(*) > 1
         )"
    )
}

/// `searching` -> `finished`: the active canonical is present and every other
/// member has been resolved (checked/errored or already promoted out).
/// Returns the number of groups transitioned.
pub fn searching_to_finished(conn: &mut Connection, eager_filter: bool) -> Result<u64> {
    let phase = prev_phase(eager_filter);
    let tx = conn.transaction().to_panic()?;
    let n = tx.execute(
        &format!(
            "UPDATE dedup_progress SET state = 'finished'
             WHERE state = 'searching'
               AND (sha1, size) IN (SELECT sha1, size FROM files
                   WHERE phase = '{}'
                   GROUP BY sha1, size
                   HAVING COUNT(*) > 0
                       -- Has Canonical
                       AND SUM(CASE WHEN canonical_id IS NOT NULL THEN 1 ELSE 0 END) = 1
                       -- no other files than canonical or checked (second check is sound)
                       AND SUM(CASE
                           WHEN (flags & :flag_completed) != 0 THEN 1
                           WHEN canonical_id IS NOT NULL THEN 1
                           ELSE 0 END) = COUNT(*))",
            phase
        ),
        named_params! {
            ":flag_completed": FileFlag::CheckWithCanonicalCompleted.mask_i64(),
        },
    ).to_panic()?;
    tx.commit().to_panic()?;
    Ok(n as u64)
}

/// `finished` -> `errored`: every non-canonical member has errored. Promote all
/// remaining members to `deduped` and clear their check flag (the error flag is
/// sticky), so the errored files still archive as their own payloads.
/// Returns `(groups errored, files promoted)`.
pub fn finish_to_error(conn: &mut Connection, eager_filter: bool) -> Result<(u64, u64)> {
    let phase = prev_phase(eager_filter);
    let tx = conn.transaction().to_panic()?;

    let n = tx.execute(
        &format!(
            "UPDATE dedup_progress SET state = 'errored'
             WHERE state = 'finished'
               AND (sha1, size) IN (SELECT sha1, size FROM files
                   WHERE phase = '{}'
                   GROUP BY sha1, size
                   HAVING COUNT(*) > 0
                       -- all remaining errored
                       AND SUM(CASE WHEN (flags & :error_flag) != 0 THEN 1 ELSE 0 END)
                           = COUNT(*) - 1
                       -- at least one error present
                       AND SUM(CASE WHEN (flags & :error_flag) != 0 THEN 1 ELSE 0 END) > 0
                       -- has canonical (criteria for 'finished')
                       AND SUM(CASE WHEN canonical_id IS NOT NULL THEN 1 ELSE 0 END) = 1)",
            phase
        ),
        named_params! {
            ":error_flag": FileFlag::ErrorWhileDedup.mask_i64(),
        },
    ).to_panic()?;
    // Group errored out -> promote all remaining members to deduped, unset the
    // check flag.
    let n2 = tx.execute(
        "UPDATE files SET phase = 'deduped', flags = flags & ~:check_flag
         WHERE (sha1, size) IN (SELECT sha1, size
             FROM dedup_progress
             WHERE state = 'errored')",
        named_params! {
            ":check_flag": FileFlag::CheckWithCanonicalCompleted.mask_i64(),
        },
    ).to_panic()?;
    tx.commit().to_panic()?;
    Ok((n as u64, n2 as u64))
}

/// `finished` -> `done`: no checked member remains (the canonical is the only
/// left member — every other candidate resolved to equal). Promote all
/// remaining members to `deduped`, unset their check flag.
/// Returns the number of files promoted.
pub fn finish_to_done(conn: &mut Connection, eager_filter: bool) -> Result<u64> {
    let phase = prev_phase(eager_filter);
    let tx = conn.transaction().to_panic()?;

    tx.execute(
        &format!(
            "UPDATE dedup_progress SET state = 'done'
             WHERE state = 'finished'
               AND (sha1, size) IN (SELECT sha1, size FROM files
                   WHERE phase = '{}'
                   GROUP BY sha1, size
                   HAVING COUNT(*) > 0
                       -- no remaining checked
                       AND SUM(CASE WHEN (flags & :check_flag) != 0 THEN 1 ELSE 0 END) = 0
                       -- has canonical (criteria for 'finished')
                       AND SUM(CASE WHEN canonical_id IS NOT NULL THEN 1 ELSE 0 END) = 1)",
            phase
        ),
        named_params! {
            ":check_flag": FileFlag::CheckWithCanonicalCompleted.mask_i64(),
        },
    ).to_panic()?;
    let n2 = tx.execute(
        "UPDATE files SET phase = 'deduped', flags = flags & ~:check_flag
         WHERE (sha1, size) IN (SELECT sha1, size
             FROM dedup_progress
             WHERE state = 'done')",
        named_params! {
            ":check_flag": FileFlag::CheckWithCanonicalCompleted.mask_i64(),
        },
    ).to_panic()?;
    tx.commit().to_panic()?;
    Ok(n2 as u64)
}

/// `finished` -> `ready`: at least one checked member remains and at least one
/// member survives without the error flag. Clears the whole group's check flag
/// and retires the active canonical to `deduped`; the next `ready_to_searching`
/// elects a fresh canonical.
/// Returns the number of canonicals promoted.
pub fn finish_to_ready(conn: &mut Connection, eager_filter: bool) -> Result<u64> {
    let phase = prev_phase(eager_filter);
    let tx = conn.transaction().to_panic()?;
    let check_bit = FileFlag::CheckWithCanonicalCompleted.mask_i64();
    let error_bit = FileFlag::ErrorWhileDedup.mask_i64();

    tx.execute(
        &format!(
            "UPDATE dedup_progress SET state = 'ready'
             WHERE state = 'finished'
               AND (sha1, size) IN (SELECT sha1, size FROM files
                   WHERE phase = '{}'
                   GROUP BY sha1, size
                   HAVING COUNT(*) > 0
                       -- at least one remaining to compare
                       AND SUM(CASE WHEN (flags & :check_flag) != 0 THEN 1 ELSE 0 END) > 0
                       -- at least one remaining without error
                       AND SUM(CASE WHEN (flags & :error_flag) = 0 THEN 1 ELSE 0 END) > 0
                       -- has canonical (criteria for 'finished')
                       AND SUM(CASE WHEN canonical_id IS NOT NULL THEN 1 ELSE 0 END) = 1)",
            phase
        ),
        named_params! {
            ":check_flag": check_bit,
            ":error_flag": error_bit,
        },
    ).to_panic()?;
    // Group has remaining members -> unset the checked flag for the whole group.
    tx.execute(
        "UPDATE files SET flags = flags & ~:check_flag
         WHERE (sha1, size) IN (SELECT sha1, size
             FROM dedup_progress
             WHERE state = 'ready')",
        named_params! { ":check_flag": check_bit },
    ).to_panic()?;
    // Promote the current canonical to deduped to retire it and search for a
    // new canonical.
    let n3 = tx.execute(
        &format!(
            "UPDATE files SET phase = 'deduped'
             WHERE phase = '{}'
               AND canonical_id = id
               AND (sha1, size) IN (SELECT sha1, size
                   FROM dedup_progress
                   WHERE state = 'ready')",
            phase
        ),
        [],
    ).to_panic()?;
    tx.commit().to_panic()?;
    Ok(n3 as u64)
}

/// `ready` -> `searching`: elect the lowest-id electable member as the active
/// canonical for every `ready` group, in one transaction, then flip groups that
/// have a pending member. Groups left `ready` (lone canonical, no pending) are
/// closed: canonical promoted to `deduped`, group -> `done` (diagnoses the
/// 2-member group that compared unequal: no further canonical is possible).
/// Returns `(canonicals elected, files promoted)`.
pub fn ready_to_searching(conn: &mut Connection, eager_filter: bool) -> Result<(u64, u64)> {
    let phase = prev_phase(eager_filter);
    let tx = conn.transaction().to_panic()?;
    let check_bit = FileFlag::CheckWithCanonicalCompleted.mask_i64();
    let error_bit = FileFlag::ErrorWhileDedup.mask_i64();

    // Elect a canonical for every ready group (atomic: an elected-but-not-
    // `searching` group would be an inconsistent persisted state on restart).
    let elected = tx.execute(
        &format!(
            "UPDATE files SET canonical_id = id
             WHERE id IN (SELECT MIN(id) FROM files
                 WHERE phase = '{}'
                   AND canonical_id IS NULL
                   AND (flags & :error_bit) = 0
                   AND (flags & :check_bit) = 0
                   -- only update the ready groups
                   AND (sha1, size) IN (SELECT sha1, size
                       FROM dedup_progress
                       WHERE state = 'ready')
                 GROUP BY sha1, size)",
            phase
        ),
        named_params! {
            ":error_bit": error_bit,
            ":check_bit": check_bit,
        },
    ).to_panic()?;
    // Flip the groups with an elected canonical and at least one pending file.
    tx.execute(
        &format!(
            "UPDATE dedup_progress SET state = 'searching'
             WHERE state = 'ready'
               AND (sha1, size) IN (SELECT sha1, size
                   FROM files
                   WHERE phase = '{}'
                   GROUP BY sha1, size
                   -- Check for canonical presence
                   HAVING SUM(CASE WHEN canonical_id = id THEN 1 ELSE 0 END) = 1
                       -- and at least one pending file
                       AND SUM(CASE WHEN canonical_id IS NULL
                           AND (flags & :check_bit) = 0 THEN 1 ELSE 0 END) > 0)",
            phase
        ),
        named_params! { ":check_bit": check_bit },
    ).to_panic()?;
    // Any group still 'ready' elected a lone canonical (no pending member):
    // promote it and close the group.
    let promoted = tx.execute(
        &format!(
            "UPDATE files SET phase = 'deduped'
             WHERE phase = '{}'
               AND canonical_id = id
               AND (sha1, size) IN (SELECT sha1, size
                   FROM dedup_progress
                   WHERE state = 'ready')",
            phase
        ),
        [],
    ).to_panic()?;
    tx.execute(
        "UPDATE dedup_progress SET state = 'done' WHERE state = 'ready'",
        [],
    ).to_panic()?;
    tx.commit().to_panic()?;
    Ok((elected as u64, promoted as u64))
}

/// Next slice of `(candidate, active canonical)` pairs still awaiting compare,
/// ordered by candidate id. The scan is restartable: candidates currently in
/// flight are excluded via `dedup_inflight`, so pulling from the top again is
/// exactly-once. Candidates may carry the error flag (they are still worth
/// comparing against this canonical); only canonical election excludes them.
pub fn list_pending_comparisons<R: SqlFileRow>(
    conn: &Connection,
    eager_filter: bool,
    last_candidate_id: u64,
    limit: u64,
) -> Result<Vec<(R, R)>> {
    let phase = prev_phase(eager_filter);
    let cand_cols = R::sql_columns(Some("cand"));
    let canon_cols = R::sql_columns(Some("canon"));
    let sql = format!(
        "SELECT {cand_cols}, {canon_cols}
         FROM files AS cand
         JOIN dedup_progress AS dp
              ON dp.sha1 = cand.sha1 AND dp.size = cand.size AND dp.state = 'searching'
         JOIN files AS canon
              ON canon.sha1 = cand.sha1 AND canon.size = cand.size
             AND canon.canonical_id = canon.id AND canon.phase = '{phase}'
         WHERE cand.phase = '{phase}'
           AND cand.canonical_id IS NULL
           AND (cand.flags & :check_flag) = 0
           AND cand.id > :last
           AND NOT EXISTS (SELECT 1 FROM dedup_inflight di WHERE di.candidate_id = cand.id)
         ORDER BY cand.id
         LIMIT :limit"
    );
    let mut stmt = conn.prepare(&sql).to_panic()?;
    let rows = stmt
        .query_map(
            named_params! {
                ":check_flag": FileFlag::CheckWithCanonicalCompleted.mask_i64(),
                ":last": last_candidate_id as i64,
                ":limit": limit,
            },
            |row| {
                let cand = R::from_row(row, Some("cand"))?;
                let canon = R::from_row(row, Some("canon"))?;
                Ok((cand, canon))
            },
        )
        .to_panic()?;
    rows.collect::<rusqlite::Result<Vec<_>>>().to_panic().map_err(Into::into)
}

/// Mark candidates as in-flight (handed to a worker, outcome not yet applied).
/// Idempotent. Callers wrap batches in a transaction.
pub fn mark_inflight(conn: &Connection, ids: &[FileId]) -> Result<()> {
    let mut stmt = conn
        .prepare("INSERT OR IGNORE INTO dedup_inflight (candidate_id) VALUES (:id)")
        .to_panic()?;
    for id in ids {
        stmt.execute(named_params! { ":id": id.0 }).to_panic()?;
    }
    Ok(())
}

/// Clear in-flight markers once the matching outcome is applied. Idempotent.
pub fn unmark_inflight(conn: &Connection, ids: &[FileId]) -> Result<()> {
    let mut stmt = conn
        .prepare("DELETE FROM dedup_inflight WHERE candidate_id = :id")
        .to_panic()?;
    for id in ids {
        stmt.execute(named_params! { ":id": id.0 }).to_panic()?;
    }
    Ok(())
}

/// Set canonical column of the row identified by file_id
pub fn set_canonical(conn: &Connection, file_id: FileId, canonical_id: FileId) -> Result<()> {
    conn.execute(
        "UPDATE files SET canonical_id = :canonical_id, phase = 'deduped' WHERE id = :id",
        named_params! {
            ":canonical_id": canonical_id.0,
            ":id": file_id.0,
        },
    ).to_panic()?;
    Ok(())
}

// INFO: Testing
/// Update canonical column of a row to the row's file_id. Filter row by file_id
pub fn mark_self_canonical(conn: &Connection, file_id: FileId) -> Result<()> {
    conn.execute(
        "UPDATE files SET canonical_id = id, phase = 'deduped' WHERE id = :id",
        named_params! { ":id": file_id.0 },
    ).to_panic()?;
    Ok(())
}

pub fn promote_non_ineligible_entries_to_dedup(conn: &Connection, eager_filter: bool)
    -> Result<u64> {
    let filter_query = generate_archive_filter(None);
    // INFO: sha1 <=> err_flag
    let n = conn.execute(
        &format!(
            "UPDATE files SET phase = 'deduped'
             WHERE phase = '{}'
                 AND (ftype != 'file'
                     OR sha1 IS NULL
                     OR (flags & :sha_err) != 0
                     OR NOT ({filter_query}))",
            prev_phase(eager_filter)
        ),
        named_params! { ":sha_err": FileFlag::ErrorWhileHash.mask_i64() },
    ).to_panic()?;
    Ok(n as u64)
}

/// Unique `(sha1, size)` content: no compare round.
pub fn promote_singleton_filtered_to_deduped(conn: &Connection, eager_filter: bool) -> Result<u64> {
    let n = conn.execute(
        &format!(
            "UPDATE files SET phase = 'deduped'
         WHERE phase = '{}'
           AND sha1 IS NOT NULL
           AND (sha1, size) IN (
               SELECT sha1, size FROM files
               WHERE sha1 IS NOT NULL
               GROUP BY sha1, size
               HAVING COUNT(*) = 1
           )",
            prev_phase(eager_filter)
        ),
        [],
    ).to_panic()?;
    Ok(n as u64)
}

/// Count of files still flagged `CheckWithCanonicalCompleted` — must be 0 once
/// the phase finished (all round transitions clear it in bulk).
pub fn count_check_with_canonical_completed(conn: &Connection) -> Result<u64> {
    let bit = FileFlag::CheckWithCanonicalCompleted.mask_i64();
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM files WHERE (flags & :bit) != 0",
        named_params! { ":bit": bit },
        |row| row.get(0),
    ).to_panic()?;
    Ok(n as u64)
}


pub fn ingest_compare_outcome(
    conn: &mut Connection, results: &Vec<CompareOutcome>)
    -> Result<u64> {
    let mut resolved = 0u64;
    let tx = conn.transaction().to_panic()?;

    for outcome in results.iter() {
        // Update the modified flag on the files.
        if outcome.canonical_modified {
            set_file_flag(&tx, outcome.canonical_id, FileFlag::Modified, true)?;
        }
        if outcome.candidate_modified {
            set_file_flag(&tx, outcome.candidate_id, FileFlag::Modified, true)?;
        }
        match &outcome.equal {
            Ok(true) => {
                set_canonical(&tx, outcome.candidate_id, outcome.canonical_id)?;
                resolved += 1;
            }
            Ok(false) => {
                assert_eq!(1, set_file_flag(
                    &tx, outcome.candidate_id, FileFlag::CheckWithCanonicalCompleted, true)?);
            }
            Err((failed_id, error)) => {
                assert_eq!(1, set_file_flag(
                    &tx, outcome.candidate_id, FileFlag::CheckWithCanonicalCompleted, true)?);
                assert_eq!(1, set_file_flag(
                    &tx, *failed_id, FileFlag::ErrorWhileDedup, true)?);

            }
        }
    }
    tx.commit().to_panic()?;
    Ok(resolved)
}