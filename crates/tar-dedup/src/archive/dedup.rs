use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;

use crate::common::files::{warn_if_times_changed, PreYield};
use crate::config::Config;
use crate::db::flags::FileFlag;
use crate::db::types::{FileId, FilePhase, GroupKey, StrippedRecord};
use crate::db::Database;
use crate::error::{Error, Result};
use crate::progress::{io_buffer, CountProgress};
use crate::shutdown::Shutdown;

/// One finished compare: both keys always present.
/// `Ok(equal)` on a completed byte compare; `Err(file_id)` for the side that failed IO.
struct CompareOutcome {
    canonical_id: FileId,
    candidate_id: FileId,
    equal: std::result::Result<bool, FileId>,
}

struct ComparePair {
    canonical_id: FileId,
    candidate_id: FileId,
    canonical_path: PathBuf,
    candidate_path: PathBuf,
    canonical_mtime: Option<DateTime<Utc>>,
    canonical_atime: Option<DateTime<Utc>>,
    canonical_ctime: Option<DateTime<Utc>>,
    candidate_mtime: Option<DateTime<Utc>>,
    candidate_atime: Option<DateTime<Utc>>,
    candidate_ctime: Option<DateTime<Utc>>,
}

enum GroupPrep {
    ErroredOnly { key: GroupKey },
    Ready {
        canonical: StrippedRecord,
        candidates: Vec<StrippedRecord>,
        key: GroupKey,
    },
}

/// Build ComparePair struct from two
fn compare_pair(
    config: &Config,
    canonical: &StrippedRecord,
    candidate: &StrippedRecord,
) -> ComparePair {
    ComparePair {
        canonical_id: canonical.id,
        candidate_id: candidate.id,
        canonical_path: abs_path(config, canonical),
        candidate_path: abs_path(config, candidate),
        canonical_mtime: canonical.mtime,
        canonical_atime: canonical.atime,
        canonical_ctime: canonical.ctime,
        candidate_mtime: candidate.mtime,
        candidate_atime: candidate.atime,
        candidate_ctime: candidate.ctime,
    }
}

/// Run when `par_bridge` pulls a pair — just before compare, not in bulk upfront.
fn warn_compare_pair_times(pair: &&ComparePair) {
    warn_if_times_changed(
        &pair.canonical_path,
        pair.canonical_mtime,
        pair.canonical_atime,
        pair.canonical_ctime,
    );
    warn_if_times_changed(
        &pair.candidate_path,
        pair.candidate_mtime,
        pair.candidate_atime,
        pair.candidate_ctime,
    );
}

/// Performs the binary comparison of two files. Receives a pair of files in pair, writes results
/// into the shared results array.
/// Also updates progress bar on return and checks the shutdown command.
fn compare_one(
    pair: &ComparePair,
    shutdown: &Shutdown,
    results: &Mutex<Vec<CompareOutcome>>,
    bar: &CountProgress,
) -> Result<()> {
    shutdown.check_between_files()?;
    // Interrupt must not become a CompareOutcome (or end_round would run).
    let equal = match files_equal(&pair.canonical_path, &pair.candidate_path, shutdown) {
        Ok(v) => Ok(v),
        Err(Error::Interrupted) => return Err(Error::Interrupted),
        Err(Error::Io { path, .. }) => Err(io_error_file_id(pair, &path)),
        Err(e) => panic!("unexpected compare error (not Io/Interrupted): {e}"),
    };
    results
        .lock()
        .expect("child dedup results lock poisoned")
        .push(CompareOutcome {
            canonical_id: pair.canonical_id,
            candidate_id: pair.candidate_id,
            equal,
        });
    bar.inc(1);
    Ok(())
}

fn io_error_file_id(pair: &ComparePair, path: &Path) -> FileId {
    if path == pair.canonical_path {
        pair.canonical_id
    } else if path == pair.candidate_path {
        pair.candidate_id
    } else {
        panic!(
            "compare IO error path is neither canonical nor candidate: \
             path={} canonical={} candidate={}",
            path.display(),
            pair.canonical_path.display(),
            pair.candidate_path.display(),
        );
    }
}

fn abs_path(config: &Config, record: &StrippedRecord) -> PathBuf {
    config.input_dir.join(&record.rel_path)
}

// =================================================================================================

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    // Stub filter stage: advance hashed → filtered before dedup.
    let promoted = db.promote_hashed_to_filtered()?;
    if promoted > 0 {
        tracing::info!(count = promoted, "promoted hashed → filtered");
    }

    let catalog = db.count_files()?;
    // Early promote db entries we do not process in this phase
    let skipped_non_file = db.promote_non_file_filtered_to_deduped()?;
    let skipped_null_sha1 = db.promote_null_sha1_filtered_to_deduped()?;
    let skipped_singleton = db.promote_singleton_filtered_to_deduped()?;
    // Get actual number of our candidates.
    let candidates = db.count_files_in_phase(FilePhase::Filtered)?;

    tracing::info!(
        catalog,
        skipped_non_file,
        skipped_null_sha1,
        skipped_singleton,
        dedup_candidates = candidates,
        jobs = config.jobs,
        dedup_fail_fast = config.dedup_fail_fast,
        "dedup pass"
    );

    if candidates == 0 {
        sanity_check_flags(db)?;
        return Ok(());
    }

    // Bar tracks compare/promote workload among candidates only — not catalog size
    // or the bulk SQL skips above.
    let bar = CountProgress::with_total("dedup", candidates);
    bar.set_position(0);

    let result = run_pool(config, db, shutdown, &bar);

    match result {
        Ok(()) => {
            bar.finish("dedup complete");
            Ok(())
        }
        Err(Error::Interrupted) => {
            bar.abandon();
            Err(Error::Interrupted)
        }
        Err(e) => Err(e),
    }
}

/// Function encapsulates the iteration deduplication rounds. Structure is chosen this way as to
/// keep all thing related to the Rayon Thread Pool inside a single function.
fn run_pool(
    config: &Config,
    db: &Database,
    shutdown: &Shutdown,
    bar: &CountProgress,
) -> Result<()> {
    // TODO: Better errors.
    let pool = ThreadPoolBuilder::new()
        .num_threads(config.jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    loop {
        shutdown.check_between_files()?;

        let mut pairs: Vec<ComparePair> = Vec::new();
        let mut groups_needing_end: Vec<GroupKey> = Vec::new();

        let next_state = prepare_round(
            &mut pairs, &mut groups_needing_end,
            bar, db, config,
        )?;
        match next_state{
            (true, false) => break,
            (false, true) => continue,
            (false, false) => {},
            (true, true) => panic!("prepare_round returned (true, true). \
            All other possible values allowed. Invariant violated.")
        }

        let shutdown_workers = shutdown.clone();
        let results = Mutex::new(Vec::<CompareOutcome>::with_capacity(pairs.len()));
        // time checked = tc
        let tc_pair_iter = PreYield::new(pairs.iter(),
                                         warn_compare_pair_times);

        let parallel = pool.install(|| {
            tc_pair_iter
                .par_bridge()
                .try_for_each(|pair| compare_one(pair, &shutdown_workers, &results, bar))
        });

        // Flush finished compares either way; unfinished pairs stay pending.
        let outcomes = results.into_inner().expect("dedup results lock");
        for outcome in &outcomes {
            apply_outcome(db, outcome)?;
        }

        match parallel {
            Ok(()) => {
                // Only end the round when every scheduled pair finished.
                for key in &groups_needing_end {
                    end_round(db, key)?;
                }
            }
            Err(Error::Interrupted) => {
                tracing::warn!(
                    saved = outcomes.len(),
                    "dedup interrupted; completed compares saved, round not ended"
                );
                return Err(Error::Interrupted);
            }
            Err(e) => return Err(e),
        }
    }

    let leftover = db.count_files_in_phase(FilePhase::Filtered)?;
    // TODO this is also in the category for panic.
    if leftover != 0 {
        return Err(Error::Config(format!(
            "dedup finished with {leftover} file(s) still in filtered (expected 0 after skips + rounds)"
        )));
    }

    sanity_check_flags(db)?;
    tracing::info!("dedup complete");
    Ok(())
}

/// Perform preparations for the round loop.
/// Guarantee, either or, not both. both false -> continue loop.
/// Return type: (should-break, should-continue)
///
fn prepare_round(
    pairs: &mut Vec<ComparePair>,
    groups_needing_end: &mut Vec<GroupKey>,
    bar: &CountProgress,
    db: &Database,
    config: &Config
) -> Result<(bool, bool)> {

    let mut errored_only_groups: Vec<GroupKey> = Vec::new();
    let mut did_work = false;

    let groups = load_pending_groups(db)?;
    if groups.is_empty() {
        // break
        return Ok((true, false));
    }

    // Deal with any potentially halted progress.
    for (key, members) in groups {
        match establish_group_state(db, key, members, config.dedup_fail_fast)? {
            GroupPrep::ErroredOnly { key } => {
                errored_only_groups.push(key);
            }
            GroupPrep::Ready {
                canonical, candidates, key,
            } => {
                // Generate Candidates
                for cand in &candidates {
                    pairs.push(compare_pair(config, &canonical, cand));
                }
                groups_needing_end.push(key);
            }
        }
    }

    // Deal with errored out groups (=> no members, no candidate, no new candidates)
    for key in &errored_only_groups {
        let n = db.promote_errored_pending_to_deduped(&key.sha1, key.size)?;
        db.clear_check_with_canonical_completed(&key.sha1, key.size)?;
        bar.inc(n);
        did_work = true;
    }

    if pairs.is_empty() {
        for key in &groups_needing_end {
            end_round(db, key)?;
            did_work = true;
        }
        if !did_work {
            // break
            return Ok((true, false))
        }
        // continue
        return Ok((false, true));
    }
    Ok((false, false))
}

/// Load pending `(sha1, size)` groups and their members whose phase is Filtered.
fn load_pending_groups(db: &Database) -> Result<Vec<(GroupKey, Vec<StrippedRecord>)>> {
    let mut groups = Vec::new();
    for key in db.pending_duplicate_groups()? {
        let members: Vec<StrippedRecord> =
            db.list_filtered_in_group(&key.sha1, key.size)?;
        if members.is_empty() {
            continue;
        }
        groups.push((key, members));
    }
    Ok(groups)
}

/// Recover previous state of the group. Find the canonical. If it does not exist,
/// determine a new canonical. If no canonical exists, group is in error state.
/// Otherwise, filter the remaining files for error, CheckWithCanonicalCompleted,
/// and no canonical_id set.
fn establish_group_state(
    db: &Database,
    key: GroupKey,
    mut members: Vec<StrippedRecord>,
    fail_fast: bool,
) -> Result<GroupPrep> {

    if members.is_empty() {
        return Ok(GroupPrep::ErroredOnly { key });
    }

    // Active canonical already in this round, or elect the lowest-id electable member.
    let canonical = if let Some(i) = members
        .iter()
        .position(|m| m.canonical_id == Some(m.id))
    {
        members.swap_remove(i)
    } else {
        let elect = members
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.canonical_id.is_none() && !m.flags.get(FileFlag::ErrorWhileDedup)
            })
            .min_by_key(|(_, m)| m.id.0)
            .map(|(i, _)| i);

        match elect {
            Some(i) => {
                let mut m = members.swap_remove(i);
                db.mark_active_canonical(m.id)?;
                m.canonical_id = Some(m.id);
                m
            }
            None => {
                return Ok(GroupPrep::ErroredOnly { key });
            }
        }
    };

    // TODO Rethink fail fast: Error on compare -> Fail the round or Panic?
    let candidates: Vec<StrippedRecord> = members
        .into_iter()
        .filter(|m| {
            m.id != canonical.id
                && m.canonical_id.is_none() // INFO: This should be not needed and we could in
                                            //   theory add a this as a a panic.
                && !m.flags.get(FileFlag::CheckWithCanonicalCompleted)
                && (!fail_fast || !m.flags.get(FileFlag::ErrorWhileDedup))
        })
        .collect();

    Ok(GroupPrep::Ready { canonical, candidates, key, })
}

/// Function applied to an element of the results array.
/// Updates the candidate file on successful compare and sets the error flag to the file causing it.
fn apply_outcome(db: &Database, outcome: &CompareOutcome) -> Result<()> {
    match outcome.equal {
        Ok(true) => {
            db.set_canonical(outcome.candidate_id, outcome.canonical_id)?;
        }
        Ok(false) => {
            db.set_flag(
                outcome.candidate_id,
                FileFlag::CheckWithCanonicalCompleted,
                true,
            )?;
        }
        Err(failed) => {
            db.set_flag(failed, FileFlag::ErrorWhileDedup, true)?;
        }
    }
    Ok(())
}

/// Clean up round.
/// Promote active canonical to Deduplicated (so it no longer shows up as a
///     candidate both for next canonical and candidate for a descendent.)
/// Resets the CheckWithCanonicalCompleted flag
/// Performs error cleanup (Promote all remaining files with err flag to Deduped and reset again
/// the CheckWithCanonicalCompleted (just in case))
fn end_round(db: &Database, key: &GroupKey) -> Result<()> {
    let active = db.count_active_canonicals(&key.sha1, key.size)?;
    assert_eq!(
        active, 1,
        "end_round: expected exactly 1 active canonical in group, found {active}"
    );
    db.promote_active_canonical_in_group(&key.sha1, key.size);

    db.clear_check_with_canonical_completed(&key.sha1, key.size)?;

    if db.count_electable_pending(&key.sha1, key.size)? == 0 {
        db.promote_errored_pending_to_deduped(&key.sha1, key.size)?;
    }
    Ok(())
}

// TODO Different Error.
/// Rerun the count_check_with_canonical_completed and return an error if the count is not 0.
fn sanity_check_flags(db: &Database) -> Result<()> {
    let n = db.count_check_with_canonical_completed()?;
    if n != 0 {
        return Err(Error::Config(format!(
            "dedup sanity check failed: {n} file(s) still have CheckWithCanonicalCompleted set"
        )));
    }
    Ok(())
}

/// Check that two files are binary identical (and have same length).
/// Returns our custom Error with io variant.
fn files_equal(a: &Path, b: &Path, shutdown: &Shutdown) -> Result<bool> {
    use std::fs::File;
    use std::io::Read;

    let mut fa = File::open(a).map_err(|e| Error::io(a, e))?;
    let mut fb = File::open(b).map_err(|e| Error::io(b, e))?;

    let len_a = fa.metadata().map_err(|e| Error::io(a, e))?.len();
    let len_b = fb.metadata().map_err(|e| Error::io(b, e))?.len();
    if len_a != len_b {
        return Ok(false);
    }

    let mut buf_a = io_buffer();
    let mut buf_b = io_buffer();
    loop {
        shutdown.check_in_flight()?;
        let na = fa.read(&mut buf_a).map_err(|e| Error::io(a, e))?;
        let nb = fb.read(&mut buf_b).map_err(|e| Error::io(b, e))?;
        if na == 0 && nb == 0 {
            return Ok(true);
        }
        if na != nb || buf_a[..na] != buf_b[..nb] {
            return Ok(false);
        }
    }
}
