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
/// Idempotent. The marker set is emptied on (re-)create: a resume always
/// starts from a blank marker set (fresh process anyways), and clearing it lets
/// an in-process re-run heal markers left behind by an interrupted phase.
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
    conn.execute("DELETE FROM dedup_inflight", []).to_panic()?;
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
                       -- Has Canonical (exactly one self-canonical row)
                       AND SUM(CASE WHEN canonical_id = id THEN 1 ELSE 0 END) = 1
                       -- no other files than canonical or checked (second check is sound)
                       AND SUM(CASE
                           WHEN (flags & :flag_completed) != 0 THEN 1
                           WHEN canonical_id = id THEN 1
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
                       AND SUM(CASE WHEN canonical_id = id THEN 1 ELSE 0 END) = 1)",
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
                       AND SUM(CASE WHEN canonical_id = id THEN 1 ELSE 0 END) = 1)",
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
                       AND SUM(CASE WHEN canonical_id = id THEN 1 ELSE 0 END) = 1)",
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
    // Any group still 'ready' has no in-phase member left (a lone canonical
    // was promoted above): close it. Guards on the member set so a group whose
    // only candidate failed election (e.g. all errored) is NOT closed here.
    tx.execute(
        &format!(
            "UPDATE dedup_progress SET state = 'done'
             WHERE state = 'ready'
               AND (sha1, size) NOT IN (SELECT sha1, size
                   FROM files
                   WHERE phase = '{}')",
            phase
        ),
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
            Err((failed_id, _error)) => {
                assert_eq!(1, set_file_flag(
                    &tx, outcome.candidate_id, FileFlag::CheckWithCanonicalCompleted, true)?);
                assert_eq!(1, set_file_flag(
                    &tx, *failed_id, FileFlag::ErrorWhileDedup, true)?);

            }
        }
        // Clear the in-flight marker once the outcome is applied: dropping it
        // earlier would leave the candidate excluded from any later round's
        // `list_pending_comparisons` scan, wedging multi-round groups.
        unmark_inflight(&tx, &[outcome.candidate_id])?;
    }
    tx.commit().to_panic()?;
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use std::path::PathBuf;

    const FLAG_CHECK: i64 = 1i64 << 7;
    const FLAG_ERROR: i64 = 1i64 << 8;
    const FLAG_MODIFIED: i64 = 1i64 << 5;

    fn open_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = Connection::open(&dir.path().join("t.sqlite")).expect("open conn");
        schema::initialize(&conn).expect("schema init");
        // The internal include rule: `apply_no_filter` hands out
        // `include_reason_archive = -1`, which must satisfy the FK.
        conn.execute(
            "INSERT OR IGNORE INTO filter_reason_archive (id, source, line, expression) \
             VALUES (-1, 'internal', NULL, '.*')",
            [],
        ).expect("seed internal include rule");
        (dir, conn)
    }

    fn seed_file(conn: &Connection, id: i64, abs_path: &str, sha1_byte: u8, size: u64) {
        conn.execute(
            "INSERT INTO files (id, abs_path, ext, size, ftype, phase, sha1, \
             include_reason_archive, exclude_reason_archive, flags, canonical_id, dev, inode) \
             VALUES (:id, :abs_path, '.bin', :size, 'file', 'filtered', :sha1, -1, 0, 0, NULL, NULL, NULL)",
            named_params! {
                ":id": id,
                ":abs_path": abs_path,
                ":size": size as i64,
                ":sha1": [sha1_byte; 20].as_slice(),
            },
        ).expect("insert row");
    }

    fn seed_group(conn: &Connection, sha1_byte: u8, size: u64, state: &str) {
        conn.execute(
            "INSERT OR REPLACE INTO dedup_progress (sha1, size, state) \
             VALUES (:sha1, :size, :state)",
            named_params! {
                ":sha1": [sha1_byte; 20].as_slice(),
                ":size": size as i64,
                ":state": state,
            },
        ).expect("seed group");
    }

    fn set_canonical_self(conn: &Connection, id: i64) {
        conn.execute(
            "UPDATE files SET canonical_id = :id WHERE id = :id",
            named_params! { ":id": id },
        ).expect("set self canonical");
    }

    fn set_phase(conn: &Connection, id: i64, phase: &str) {
        conn.execute(
            "UPDATE files SET phase = :phase WHERE id = :id",
            named_params! { ":id": id, ":phase": phase },
        ).expect("set phase");
    }

    fn row_phase(conn: &Connection, id: i64) -> String {
        conn.query_row(
            "SELECT phase FROM files WHERE id = :id",
            named_params! { ":id": id },
            |row| row.get::<_, String>(0),
        ).expect("read phase")
    }

    fn row_canonical(conn: &Connection, id: i64) -> Option<i64> {
        conn.query_row(
            "SELECT canonical_id FROM files WHERE id = :id",
            named_params! { ":id": id },
            |row| row.get::<_, Option<i64>>(0),
        ).expect("read canonical_id")
    }

    fn row_flags(conn: &Connection, id: i64) -> i64 {
        conn.query_row(
            "SELECT flags FROM files WHERE id = :id",
            named_params! { ":id": id },
            |row| row.get(0),
        ).expect("read flags")
    }

    fn group_state(conn: &Connection, sha1_byte: u8, size: u64) -> String {
        conn.query_row(
            "SELECT state FROM dedup_progress WHERE sha1 = :sha1 AND size = :size",
            named_params! {
                ":sha1": [sha1_byte; 20].as_slice(),
                ":size": size as i64,
            },
            |row| row.get::<_, String>(0),
        ).expect("read group state")
    }

    fn pending_ids(conn: &Connection, last: u64, limit: u64) -> Vec<(i64, i64)> {
        list_pending_comparisons::<StrippedRecord>(conn, false, last, limit)
            .expect("list pending")
            .into_iter()
            .map(|pair| (pair.0.id.0, pair.1.id.0))
            .collect::<Vec<(i64, i64)>>()
    }

    fn inflight_count(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM dedup_inflight", [], |row| row.get(0),
        ).expect("inflight count")
    }

    #[test]
    fn ready_to_searching_elects_min_and_flips() {
        let (_dir, mut conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_file(&conn, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn, 2, "/tmp/b.bin", 7, 100);
        seed_file(&conn, 3, "/tmp/c.bin", 7, 100);
        seed_group(&conn, 7, 100, "ready");

        let (elected, promoted) = ready_to_searching(&mut conn, false).expect("transition");

        assert_eq!(elected, 1);
        assert_eq!(promoted, 0);
        assert_eq!(row_canonical(&conn, 1), Some(1));
        assert_eq!(row_canonical(&conn, 2), None);
        assert_eq!(row_canonical(&conn, 3), None);
        assert_eq!(group_state(&conn, 7, 100), "searching");
    }

    #[test]
    fn ready_to_searching_two_member_group() {
        let (_dir, mut conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_file(&conn, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn, 2, "/tmp/b.bin", 7, 100);
        seed_group(&conn, 7, 100, "ready");
        // 1 is a retired self-canonical; the lone remaining member is elected
        // then promoted out and the group closes.
        set_canonical_self(&conn, 1);
        set_phase(&conn, 1, "deduped");

        let (elected, promoted) = ready_to_searching(&mut conn, false).expect("transition");

        assert_eq!(elected, 1);
        assert_eq!(promoted, 1);
        assert_ne!(row_canonical(&conn, 2), None);
        assert_eq!(row_phase(&conn, 2), "deduped");
        assert_eq!(group_state(&conn, 7, 100), "done");
    }

    #[test]
    fn ready_to_searching_no_electable_marks_done_unreachable_guard() {
        // I5: the blanket ready->done flip must not close a group whose
        // members are still in-phase (here: all errored, none electable).
        let (_dir, mut conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_file(&conn, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn, 2, "/tmp/b.bin", 7, 100);
        seed_group(&conn, 7, 100, "ready");
        set_file_flag(&conn, FileId(1), FileFlag::ErrorWhileDedup, true).expect("flag");
        set_file_flag(&conn, FileId(2), FileFlag::ErrorWhileDedup, true).expect("flag");

        let _ = ready_to_searching(&mut conn, false).expect("transition");

        assert_eq!(group_state(&conn, 7, 100), "ready");
        assert_eq!(count_pending_dedup_groups(&conn).expect("pending"), 1);
    }

    #[test]
    fn searching_to_finished_fires_when_all_resolved() {
        let (_dir, mut conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_file(&conn, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn, 2, "/tmp/b.bin", 7, 100);
        seed_file(&conn, 3, "/tmp/c.bin", 7, 100);
        seed_group(&conn, 7, 100, "searching");
        set_canonical_self(&conn, 1);
        set_file_flag(&conn, FileId(2), FileFlag::CheckWithCanonicalCompleted, true).expect("flag");
        set_file_flag(&conn, FileId(3), FileFlag::CheckWithCanonicalCompleted, true).expect("flag");

        assert_eq!(searching_to_finished(&mut conn, false).expect("transition"), 1);
        assert_eq!(group_state(&conn, 7, 100), "finished");

        // negative: one unresolved member blocks the flip
        let (_dir2, mut conn2) = open_db();
        create_temp_dedup_table(&conn2).expect("create temp tables");
        seed_file(&conn2, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn2, 2, "/tmp/b.bin", 7, 100);
        seed_file(&conn2, 3, "/tmp/c.bin", 7, 100);
        seed_group(&conn2, 7, 100, "searching");
        set_canonical_self(&conn2, 1);
        set_file_flag(&conn2, FileId(2), FileFlag::CheckWithCanonicalCompleted, true).expect("flag");

        assert_eq!(searching_to_finished(&mut conn2, false).expect("transition"), 0);
        assert_eq!(group_state(&conn2, 7, 100), "searching");
    }

    #[test]
    fn searching_to_finished_requires_exactly_one_self_canonical() {
        // I4: the guard is `canonical_id = id`; two self-canonicals or none
        // must both block the transition.
        let (_dir, mut conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_file(&conn, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn, 2, "/tmp/b.bin", 7, 100);
        seed_file(&conn, 3, "/tmp/c.bin", 7, 100);
        seed_group(&conn, 7, 100, "searching");
        set_canonical_self(&conn, 1);
        set_canonical_self(&conn, 2);
        set_file_flag(&conn, FileId(3), FileFlag::CheckWithCanonicalCompleted, true).expect("flag");

        assert_eq!(searching_to_finished(&mut conn, false).expect("transition"), 0);
        assert_eq!(group_state(&conn, 7, 100), "searching");

        let (_dir2, mut conn2) = open_db();
        create_temp_dedup_table(&conn2).expect("create temp tables");
        seed_file(&conn2, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn2, 2, "/tmp/b.bin", 7, 100);
        seed_file(&conn2, 3, "/tmp/c.bin", 7, 100);
        seed_group(&conn2, 7, 100, "searching");
        set_file_flag(&conn2, FileId(1), FileFlag::CheckWithCanonicalCompleted, true).expect("flag");
        set_file_flag(&conn2, FileId(2), FileFlag::CheckWithCanonicalCompleted, true).expect("flag");
        set_file_flag(&conn2, FileId(3), FileFlag::CheckWithCanonicalCompleted, true).expect("flag");

        assert_eq!(searching_to_finished(&mut conn2, false).expect("transition"), 0);
        assert_eq!(group_state(&conn2, 7, 100), "searching");
    }

    #[test]
    fn finish_to_done_promotes_and_all_have_canonical() {
        let (_dir, mut conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_file(&conn, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn, 2, "/tmp/b.bin", 7, 100);
        seed_file(&conn, 3, "/tmp/c.bin", 7, 100);
        seed_group(&conn, 7, 100, "finished");
        set_canonical_self(&conn, 1);
        set_canonical(&conn, FileId(2), FileId(1)).expect("link 2");
        set_canonical(&conn, FileId(3), FileId(1)).expect("link 3");

        let promoted = finish_to_done(&mut conn, false).expect("transition");

        // SQLite counts rows matched by the group promote, including the two
        // candidates already linked/promoted out.
        assert_eq!(promoted, 3);
        assert_eq!(group_state(&conn, 7, 100), "done");
        assert_eq!(row_phase(&conn, 1), "deduped");
        assert_eq!(row_canonical(&conn, 1), Some(1));
        assert_eq!(row_canonical(&conn, 2), Some(1));
        assert_eq!(row_canonical(&conn, 3), Some(1));

        // negative: an unresolved checked member blocks it
        let (_dir2, mut conn2) = open_db();
        create_temp_dedup_table(&conn2).expect("create temp tables");
        seed_file(&conn2, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn2, 2, "/tmp/b.bin", 7, 100);
        seed_group(&conn2, 7, 100, "finished");
        set_canonical_self(&conn2, 1);
        set_file_flag(&conn2, FileId(2), FileFlag::CheckWithCanonicalCompleted, true).expect("flag");

        assert_eq!(finish_to_done(&mut conn2, false).expect("transition"), 0);
        assert_eq!(group_state(&conn2, 7, 100), "finished");
    }

    #[test]
    fn finish_to_error_errored_with_all_errored_candidates() {
        let (_dir, mut conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_file(&conn, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn, 2, "/tmp/b.bin", 7, 100);
        seed_file(&conn, 3, "/tmp/c.bin", 7, 100);
        seed_group(&conn, 7, 100, "finished");
        set_canonical_self(&conn, 1);
        for id in [2, 3] {
            set_file_flag(&conn, FileId(id), FileFlag::CheckWithCanonicalCompleted, true).expect("flag");
            set_file_flag(&conn, FileId(id), FileFlag::ErrorWhileDedup, true).expect("flag");
        }

        let (errored, promoted) = finish_to_error(&mut conn, false).expect("transition");

        assert_eq!(errored, 1);
        assert_eq!(promoted, 3);
        assert_eq!(group_state(&conn, 7, 100), "errored");
        assert_eq!(row_canonical(&conn, 2), None);
        assert_ne!(row_flags(&conn, 2) & FLAG_ERROR, 0);
        assert_eq!(row_flags(&conn, 2) & FLAG_CHECK, 0);
        assert_eq!(row_phase(&conn, 2), "deduped");

        // negative: a candidate without the error flag blocks it
        let (_dir2, mut conn2) = open_db();
        create_temp_dedup_table(&conn2).expect("create temp tables");
        seed_file(&conn2, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn2, 2, "/tmp/b.bin", 7, 100);
        seed_file(&conn2, 3, "/tmp/c.bin", 7, 100);
        seed_group(&conn2, 7, 100, "finished");
        set_canonical_self(&conn2, 1);
        set_file_flag(&conn2, FileId(2), FileFlag::CheckWithCanonicalCompleted, true).expect("flag");
        set_file_flag(&conn2, FileId(2), FileFlag::ErrorWhileDedup, true).expect("flag");
        set_file_flag(&conn2, FileId(3), FileFlag::CheckWithCanonicalCompleted, true).expect("flag");

        assert_eq!(finish_to_error(&mut conn2, false).expect("transition").0, 0);
        assert_eq!(group_state(&conn2, 7, 100), "finished");
    }

    #[test]
    fn finish_to_ready_retires_canonical_and_clears_flags() {
        let (_dir, mut conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_file(&conn, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn, 2, "/tmp/b.bin", 7, 100);
        seed_file(&conn, 3, "/tmp/c.bin", 7, 100);
        seed_group(&conn, 7, 100, "finished");
        set_canonical_self(&conn, 1);
        set_file_flag(&conn, FileId(2), FileFlag::CheckWithCanonicalCompleted, true).expect("flag");
        set_file_flag(&conn, FileId(3), FileFlag::CheckWithCanonicalCompleted, true).expect("flag");
        set_file_flag(&conn, FileId(3), FileFlag::ErrorWhileDedup, true).expect("flag");

        let promoted = finish_to_ready(&mut conn, false).expect("transition");

        assert_eq!(promoted, 1);
        assert_eq!(group_state(&conn, 7, 100), "ready");
        assert_eq!(row_phase(&conn, 1), "deduped");
        assert_eq!(row_phase(&conn, 2), "filtered");
        assert_eq!(row_phase(&conn, 3), "filtered");
        assert_eq!(row_flags(&conn, 2) & FLAG_CHECK, 0);
        assert_eq!(row_flags(&conn, 3) & FLAG_CHECK, 0);
        assert_ne!(row_flags(&conn, 3) & FLAG_ERROR, 0);

        // negative: no checked member -> the done path, not ready
        let (_dir2, mut conn2) = open_db();
        create_temp_dedup_table(&conn2).expect("create temp tables");
        seed_file(&conn2, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn2, 2, "/tmp/b.bin", 7, 100);
        seed_group(&conn2, 7, 100, "finished");
        set_canonical_self(&conn2, 1);
        set_canonical(&conn2, FileId(2), FileId(1)).expect("link 2");

        assert_eq!(finish_to_ready(&mut conn2, false).expect("transition"), 0);
        assert_eq!(group_state(&conn2, 7, 100), "finished");
    }

    #[test]
    fn fsm_roundtrip_two_member_unequal_terminates_done() {
        let (_dir, mut conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_file(&conn, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn, 2, "/tmp/b.bin", 7, 100);
        seed_group(&conn, 7, 100, "ready");

        assert_eq!(ready_to_searching(&mut conn, false).expect("r2s").0, 1);
        assert_eq!(group_state(&conn, 7, 100), "searching");

        let mut results = Vec::<CompareOutcome>::new();
        results.push(CompareOutcome {
            canonical_id: FileId(1),
            candidate_id: FileId(2),
            equal: Ok(false),
            canonical_modified: false,
            candidate_modified: false,
        });
        assert_eq!(ingest_compare_outcome(&mut conn, &results).expect("ingest"), 0);
        assert_eq!(searching_to_finished(&mut conn, false).expect("s2f"), 1);
        assert_eq!(finish_to_ready(&mut conn, false).expect("f2r"), 1);
        assert_eq!(ready_to_searching(&mut conn, false).expect("r2s"), (1, 1));
        // lonely 2 was elected + promoted; the guarded done-flip closes it.
        assert_eq!(group_state(&conn, 7, 100), "done");
        assert_eq!(row_canonical(&conn, 1), Some(1));
        assert_eq!(row_canonical(&conn, 2), Some(2));
        assert_eq!(row_phase(&conn, 1), "deduped");
        assert_eq!(row_phase(&conn, 2), "deduped");
        assert_eq!(count_pending_dedup_groups(&conn).expect("pending"), 0);
    }

    #[test]
    fn fsm_roundtrip_three_member_unequal_than_equal_terminates_done() {
        // I1 repro at the db level: round-1 in-flight markers must be cleared
        // by ingest, or round 2's list_pending_comparisons excludes candidate 3.
        let (_dir, mut conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_file(&conn, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn, 2, "/tmp/b.bin", 7, 100);
        seed_file(&conn, 3, "/tmp/c.bin", 7, 100);
        seed_group(&conn, 7, 100, "searching");
        set_canonical_self(&conn, 1);

        // Round 1: candidates 2 and 3 compare unequal vs 1.
        mark_inflight(&conn, &[FileId(2), FileId(3)]).expect("mark round 1");
        let mut round1 = Vec::<CompareOutcome>::new();
        round1.push(CompareOutcome {
            canonical_id: FileId(1), candidate_id: FileId(2), equal: Ok(false),
            canonical_modified: false, candidate_modified: false,
        });
        round1.push(CompareOutcome {
            canonical_id: FileId(1), candidate_id: FileId(3), equal: Ok(false),
            canonical_modified: false, candidate_modified: false,
        });
        ingest_compare_outcome(&mut conn, &round1).expect("round 1 ingest");
        assert_eq!(inflight_count(&conn), 0);
        assert_eq!(searching_to_finished(&mut conn, false).expect("s2f"), 1);
        assert_eq!(finish_to_ready(&mut conn, false).expect("f2r"), 1);

        // Round 2 clippage: candidate 3 alone must still be listed.
        assert_eq!(ready_to_searching(&mut conn, false).expect("r2s").0, 1);
        assert_eq!(group_state(&conn, 7, 100), "searching");
        let round2 = pending_ids(&conn, 0, 100);
        assert_eq!(round2, [(3, 2)]);

        // Round 2: 3 compares equal vs the new canonical 2.
        mark_inflight(&conn, &[FileId(3)]).expect("mark round 2");
        let mut round2_res = Vec::<CompareOutcome>::new();
        round2_res.push(CompareOutcome {
            canonical_id: FileId(2), candidate_id: FileId(3), equal: Ok(true),
            canonical_modified: false, candidate_modified: false,
        });
        assert_eq!(ingest_compare_outcome(&mut conn, &round2_res).expect("round 2 ingest"), 1);
        assert_eq!(inflight_count(&conn), 0);
        assert_eq!(searching_to_finished(&mut conn, false).expect("s2f"), 1);
        assert_eq!(finish_to_done(&mut conn, false).expect("f2d"), 3);

        assert_eq!(group_state(&conn, 7, 100), "done");
        assert_eq!(row_canonical(&conn, 1), Some(1));
        assert_eq!(row_canonical(&conn, 2), Some(2));
        assert_eq!(row_canonical(&conn, 3), Some(2));
        assert_eq!(row_phase(&conn, 1), "deduped");
        assert_eq!(row_phase(&conn, 2), "deduped");
        assert_eq!(row_phase(&conn, 3), "deduped");
        assert_eq!(count_pending_dedup_groups(&conn).expect("pending"), 0);
    }

    #[test]
    fn ingest_compare_outcome_true_links_and_resolves() {
        let (_dir, mut conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_file(&conn, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn, 2, "/tmp/b.bin", 7, 100);
        mark_inflight(&conn, &[FileId(2)]).expect("mark");

        let mut results = Vec::<CompareOutcome>::new();
        results.push(CompareOutcome {
            canonical_id: FileId(1), candidate_id: FileId(2), equal: Ok(true),
            canonical_modified: false, candidate_modified: false,
        });
        let resolved = ingest_compare_outcome(&mut conn, &results).expect("ingest");

        assert_eq!(resolved, 1);
        assert_eq!(row_canonical(&conn, 2), Some(1));
        assert_eq!(row_phase(&conn, 2), "deduped");
        assert_eq!(inflight_count(&conn), 0);
    }

    #[test]
    fn ingest_compare_outcome_false_sets_check_flag() {
        let (_dir, mut conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_file(&conn, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn, 2, "/tmp/b.bin", 7, 100);
        mark_inflight(&conn, &[FileId(2)]).expect("mark");

        let mut results = Vec::<CompareOutcome>::new();
        results.push(CompareOutcome {
            canonical_id: FileId(1), candidate_id: FileId(2), equal: Ok(false),
            canonical_modified: false, candidate_modified: false,
        });
        let resolved = ingest_compare_outcome(&mut conn, &results).expect("ingest");

        assert_eq!(resolved, 0);
        assert_ne!(row_flags(&conn, 2) & FLAG_CHECK, 0);
        assert_eq!(row_phase(&conn, 2), "filtered");
        assert_eq!(inflight_count(&conn), 0);
    }

    #[test]
    fn ingest_compare_outcome_error_sets_flags_on_failed_side() {
        let (_dir, mut conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_file(&conn, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn, 2, "/tmp/b.bin", 7, 100);
        mark_inflight(&conn, &[FileId(2)]).expect("mark");

        let mut results = Vec::<CompareOutcome>::new();
        results.push(CompareOutcome {
            canonical_id: FileId(1),
            candidate_id: FileId(2),
            equal: Err((FileId(1), FileStatError::Io {
                path: PathBuf::from("/tmp/a.bin"),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied, "denied".to_string()),
            })),
            canonical_modified: false,
            candidate_modified: false,
        });
        let resolved = ingest_compare_outcome(&mut conn, &results).expect("ingest");

        assert_eq!(resolved, 0);
        assert_ne!(row_flags(&conn, 2) & FLAG_CHECK, 0);
        assert_ne!(row_flags(&conn, 1) & FLAG_ERROR, 0);
        assert_eq!(inflight_count(&conn), 0);
    }

    #[test]
    fn ingest_compare_outcome_sets_modified_flags() {
        let (_dir, mut conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_file(&conn, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn, 2, "/tmp/b.bin", 7, 100);

        let mut results = Vec::<CompareOutcome>::new();
        results.push(CompareOutcome {
            canonical_id: FileId(1), candidate_id: FileId(2), equal: Ok(true),
            canonical_modified: true, candidate_modified: true,
        });
        ingest_compare_outcome(&mut conn, &results).expect("ingest");

        assert_ne!(row_flags(&conn, 1) & FLAG_MODIFIED, 0);
        assert_ne!(row_flags(&conn, 2) & FLAG_MODIFIED, 0);
    }

    #[test]
    fn list_pending_comparisons_slices_and_orders() {
        let (_dir, conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        for (id, name) in [(1, "/tmp/a.bin"), (2, "/tmp/b.bin"), (3, "/tmp/c.bin"), (4, "/tmp/d.bin")] {
            seed_file(&conn, id, name, 7, 100);
        }
        set_canonical_self(&conn, 1);
        seed_group(&conn, 7, 100, "searching");

        assert_eq!(pending_ids(&conn, 0, 2), [(2, 1), (3, 1)]);
        assert_eq!(pending_ids(&conn, 3, 2), [(4, 1)]);
        assert!(pending_ids(&conn, 4, 2).is_empty());
    }

    #[test]
    fn mark_unmark_inflight_excludes_and_restores() {
        let (_dir, conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        for (id, name) in [(1, "/tmp/a.bin"), (2, "/tmp/b.bin"), (3, "/tmp/c.bin"), (4, "/tmp/d.bin")] {
            seed_file(&conn, id, name, 7, 100);
        }
        set_canonical_self(&conn, 1);
        seed_group(&conn, 7, 100, "searching");

        mark_inflight(&conn, &[FileId(2), FileId(3)]).expect("mark");
        assert_eq!(pending_ids(&conn, 0, 100), [(4, 1)]);

        unmark_inflight(&conn, &[FileId(3)]).expect("unmark");
        assert_eq!(pending_ids(&conn, 0, 100), [(3, 1), (4, 1)]);

        mark_inflight(&conn, &[FileId(2)]).expect("re-mark idempotent");
        assert_eq!(pending_ids(&conn, 0, 100), [(3, 1), (4, 1)]);
    }

    #[test]
    fn list_pending_comparisons_empty_while_pending_groups_exist() {
        let (_dir, conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_file(&conn, 1, "/tmp/a.bin", 7, 100);
        seed_file(&conn, 2, "/tmp/b.bin", 7, 100);
        set_canonical_self(&conn, 1);
        seed_group(&conn, 7, 100, "searching");
        mark_inflight(&conn, &[FileId(2)]).expect("mark");

        assert!(pending_ids(&conn, 0, 100).is_empty());
        assert_eq!(count_pending_dedup_groups(&conn).expect("pending"), 1);
    }

    #[test]
    fn promote_non_ineligible_and_singleton() {
        let (_dir, conn) = open_db();
        seed_file(&conn, 1, "/tmp/single.bin", 1, 10);
        seed_file(&conn, 2, "/tmp/dir.bin", 1, 20);
        conn.execute("UPDATE files SET ftype = 'dir' WHERE id = 2", []).expect("dir ftype");
        seed_file(&conn, 3, "/tmp/nosha.bin", 1, 30);
        conn.execute("UPDATE files SET sha1 = NULL WHERE id = 3", []).expect("null sha");
        seed_file(&conn, 4, "/tmp/shaerr.bin", 1, 40);
        set_file_flag(&conn, FileId(4), FileFlag::ErrorWhileHash, true).expect("flag");
        seed_file(&conn, 5, "/tmp/excluded.bin", 1, 50);
        conn.execute("UPDATE files SET include_reason_archive = 0 WHERE id = 5", []).expect("excl");
        seed_file(&conn, 6, "/tmp/g1.bin", 7, 100);
        seed_file(&conn, 7, "/tmp/g2.bin", 7, 100);

        // run() order: ineligible promotions first, then singletons
        assert_eq!(promote_non_ineligible_entries_to_dedup(&conn, false).expect("ineligible"), 4);
        assert_eq!(promote_singleton_filtered_to_deduped(&conn, false).expect("singleton"), 1);

        assert_eq!(row_phase(&conn, 1), "deduped");
        assert_eq!(row_phase(&conn, 2), "deduped");
        assert_eq!(row_phase(&conn, 3), "deduped");
        assert_eq!(row_phase(&conn, 4), "deduped");
        assert_eq!(row_phase(&conn, 5), "deduped");
        assert_eq!(row_phase(&conn, 6), "filtered");
        assert_eq!(row_phase(&conn, 7), "filtered");
    }

    #[test]
    fn count_pending_dedup_groups_tracks_states() {
        let (_dir, conn) = open_db();
        create_temp_dedup_table(&conn).expect("create temp tables");
        seed_group(&conn, 1, 10, "ready");
        seed_group(&conn, 2, 10, "searching");
        seed_group(&conn, 3, 10, "finished");
        seed_group(&conn, 4, 10, "done");
        seed_group(&conn, 5, 10, "errored");

        assert_eq!(count_pending_dedup_groups(&conn).expect("pending"), 3);
    }
}