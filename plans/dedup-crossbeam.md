# Plan: crossbeam dedup phase + in-DB group state machine

## Goal

Rewrite `crates/tar-dedup/src/archive/dedup.rs` to drive deduplication through the SQLite
`dedup_progress` group-state machine, running the byte-compare workload over a crossbeam
producer/consumer pipeline (the same pattern as the hash phase). All SQLite stays in `db/`
(the user directed: anything rusqlite goes into `db/`); the in-memory `prepare_round`
machinery is **thrown out** for this "in db" approach.

The user "majorly refactored the database s.t. we do a lot more in db than in memory." The
compare function (`files_equal`/`compare_one`) and the larger scaffold (`ComparePair`,
`CompareOutcome`, `compare_error_file_id`, `warn_compare_pair_times`) are kept.

## Context / why (performance requirement)

From the user (verbatim intent): groups of 50 GiB files take a long time to compare. We want
**other groups to step their FSMs through several rounds while a slow group is still in its
first round**. Do NOT wait for the big files to finish round 1 before advancing other groups'
FSMs — that would lose the same performance the crossbeam hash pipeline just recovered.

This forced two design decisions (both settled below):
1. The per-group FSM transitions run **every main-loop iteration** (they are guarded SQL: a
   group only flips when its own round's candidates are all ingested), never gated on
   global "pool exhausted".
2. A `dedup_inflight` **TEMPORARY** table makes the candidate re-scan exactly-once, so the
   scan cursor can "start again" (reset to 0) at any time without double-comparing a pair
   that is still in flight on a worker.

## Non-goals (recorded, do NOT do)

- No changes to the `files` schema / no new FileFlag bits (the transient in-flight state goes
  in `dedup_inflight`, not into the persistent `files.flags`).
- No long-lived SQL cursor (same borrow-checker wall as hash: a `Statement` borrows its
  `Connection`; a struct can't own both). All SQLite calls complete inside `db/`.
- `dedup_progress` must survive for resume (regular table). Only `dedup_inflight` is a real
  SQLite TEMPORARY table (per-connection, cleared every phase automatically).
- Keep `promote_non_ineligible_entries_to_dedup` + `promote_singleton_filtered_to_deduped`
  (bulk skips before the state machine) as-is.

## Repo state / starting point

- `archive/dedup.rs` currently has the OLD in-memory round logic: `run`, `run_pool`
  (rayon `pool.install` + `prepare_round` loop), `prepare_round`, `load_pending_groups`,
  `establish_group_state`, `apply_outcome`, `end_round`, `count_active_canonicals`-driven
  helpers, `sanity_check_flags`, `files_equal`.
- `db/dedup.rs` already contains WIP state-machine functions (broken, see Fix list below):
  `create_temp_dedup_table`, `drop_temp_dedup_table`, `populate_temp_table`,
  `searching_to_finished`, `finish_to_error`, `finish_to_done`, `finish_to_ready`,
  `ready_to_searching`, plus legacy helpers (`set_canonical`, `mark_self_canonical`,
  `mark_active_canonical`, `promote_to_deduped`, `promote_singleton_filtered_to_deduped`,
  `pending_duplicate_groups`, `list_filtered_in_group`, `clear_check_with_canonical_completed`,
  `promote_errored_pending_to_deduped`, `count_check_with_canonical_completed`,
  `count_active_canonicals`, `promote_active_canonical_in_group`, `count_electable_pending`).
- `db/flags.rs`: `FileFlag::CheckWithCanonicalCompleted = 7`, `FileFlag::ErrorWhileDedup = 8`
  (bits, `.mask_i64()`). `db/types.rs`: `FilePhase::{Hashed, Filtered, Deduped}`,
  `GroupKey { sha1: [u8; 20], size: u64 }`.
- Uncommitted user WIP lives in `db/dedup.rs` and may shift lines; expect to rewrite that file.

## State machine (dedup_progress, keyed by (sha1, size))

States: `ready → searching → finished → {errored | done | ready}` (repeat), exit when every
group is `done` or `errored`.

- **Ready** — initial state; no active canonical; at least one member that can be elected
  (electable = `canonical_id IS NULL` AND **no error flag**).
- **Searching** — active canonical elected (`canonical_id = id`), at least one pending row
  (`canonical_id IS NULL` AND check flag false).
- **Finished** — active canonical present AND every other group row has either found a
  canonical (`phase = 'deduped'`, `canonical_id` set) OR is unsuccessful (`canonical_id IS
  NULL` AND check flag set).
- **Errored** — at least one file in the group without `canonical_id` but with the error flag
  set.
- **Done** — no more electable canonicals / group empty.

Important: only files of the previous phase of a group are considered (the transitions filter
`WHERE phase = prev`); files already promoted to `deduped` leave the group's in-round set, so
the "has canonical" sums in the transition SQL see exactly one canonical.

Key user correction (dedup semantics): **candidates for COMPARISON may have the error flag
set**; only candidates for the NEXT CANONICAL election cannot have it. (Error is per-pair; a
file that errored against one canonical can still be compared against another.) The error
flag's real purpose is to prevent thrashing the DB by repeatedly re-electing a canonical that
re-reads and fails.

Edge case (settled): a 2-member group whose files compare unequal (stale sha1 after a
mid-run modification, or sha1 collision). Round 1 leaves one member; when `ready_to_searching`
elects it as the lone canonical with **no remaining pending candidate**, promote it to
`deduped` and set the group `state='done'` directly (prevents an infinite spin).

## DDL (db/dedup.rs)

```sql
CREATE TABLE IF NOT EXISTS dedup_progress (
    sha1  BLOB  NOT NULL,                       -- matches files.sha1 (BLOB)
    size  INTEGER NOT NULL CHECK(size > 0),
    state TEXT NOT NULL CHECK(state IN ('ready','searching','finished','errored','done'))
        DEFAULT 'ready',
    PRIMARY KEY (sha1, size)
);

CREATE TEMP TABLE IF NOT EXISTS dedup_inflight (    -- per-connection scratch; cleared each phase
    candidate_id INTEGER PRIMARY KEY
);

DROP TABLE IF EXISTS dedup_progress;               -- fix the current `IF NOT EXISTS` typo
```

Notes:
- `dedup_progress.sha1` is BLOB to JOIN directly with `files.sha1` (the current TEXT stub is
  broken for JOINs).
- `dedup_inflight` needs no drop and no explicit clear: it is a SQLite TEMPORARY table,
  visible only on the creating connection, and gone when the connection closes. Each run /
  resume therefore starts with it empty — which is exactly correct, because a compare that
  was in flight at a crash/interrupt was never applied (ingest is transactional), so those
  candidates must be re-listed and re-compared.
- `ready_to_searching` must run election + `ready → searching` flip in ONE transaction (an
  elected-but-not-`searching` canonical would be an inconsistent persisted state on restart).

## Control flow (archive/dedup.rs)

```
create_temp_dedup_table(); populate_temp_table(eager)      // idempotent
ready_to_searching(eager)                                  // seed first wave
spawn io_jobs compare workers (create_thread_bars(BarKind::Bytes, io_jobs))

let mut last = 0u64;                 // scan cursor over candidate ids
let mut scan_exhausted = false;
loop {
    if shutdown.check_between_files().is_err() { interrupted = true; break }
    let mut busy = false;

    // 1) drain + ingest
    while pending_out.len() < DRAIN_CHUNK {
        match out_r.try_recv() {
            Ok(outcome) => { pending_out.push(outcome); busy = true }
            Err(_) => break
        }
    }
    if pending_out.len() >= DRAIN_CHUNK || (nowhere empty) {
        let n = ingest_chunk(&mut pending_out)?;   // tx: set_canonical / set checked /
                                                   //     set errored + record error; unmark in-flight
        completed += n; busy = true;
    }

    // 2) per-group FSM — runs every iteration; guarded SQL, only complete groups flip
    searching_to_finished(eager)?
    finish_to_error(eager)?              // then: if fail_fast && errored_groups > 0 -> Err(Config)
    finish_to_done(eager)?
    finish_to_ready(eager)?
    ready_to_searching(eager)?
    if count_pending_dedup_groups() == 0 { break }        // all done/errored

    // 3) feed
    if !scan_exhausted {
        batch = list_pending_comparisons::<StrippedRecord>(last, FEED_CHUNK)?
        if batch.is_empty() {
            scan_exhausted = true;
        } else {
            for (cand, canon) in batch {
                match work_s.try_send(compare_pair(&canon, &cand)) {
                    Ok(_) => { fed += 1; send_ids.push(cand.id); busy = true }
                    Err(_) => break
                }
            }
            mark_inflight(send_ids)?;                     // batched INSERT OR IGNORE
            last = feed_buf last pair's candidate id;
        }
    } else {
        // "start again" — safe: dedup_inflight excludes in-flight candidates
        last = 0; scan_exhausted = false;
    }

    if !busy { thread::sleep(Duration::from_millis(1)) }
}

drop(work_s);
// definitive drain: out_r.recv() until Disconnected; ingest remaining; (interrupt path skips FSM)
progress.drop_thread_bars();
recorder.flush()?;
// success path only:
drop_temp_dedup_table()?;
```

Termination details:
- `fed` counts pairs try_send Ok'd; `completed` counts ingested outcomes. Ordinary termination
  when `count_pending_dedup_groups()==0`. The scan cursor/inflight means no global
  "pool exhausted + fully drained" gate is needed (that gate was the perf regression).
- On interrupt: break at the loop top; drain + ingest what workers produced; **skip** the FSM
  steps; log "dedup stopped; completed compares saved"; return `Err(Error::Interrupted)`. On
  force abort, workers bail via `check_in_flight` inside `files_equal` and send no outcome.

### Worker

```rust
fn compare_worker(
    bar: ProgressBar,                    // by value (create_thread_bars + thread_bar(0..n-1))
    shutdown: Shutdown,                  // clone per spawn
    work: Receiver<ComparePair>,
    out: Sender<CompareOutcome>,
) {
    let mut buf_a = Vec::<u8>::new();    // sized to io_buffer() once, reused
    let mut buf_b = Vec::<u8>::new();
    for pair in work recv loop {
        if shutdown.check_between_files().is_err() { break }
        bar.reset(); bar.set_message/bar.set_length(...);
        warn_if_times_changed(...canonical...);
        warn_if_times_changed(...candidate...);
        let res = compare_one(&pair, &shutdown, &mut buf_a, &mut buf_b, detect_hardlinks);
        match res {
            Ok(outcome) => out.send(outcome).expect(...),
            Err(Error::Interrupted) => break,          // force abort: no outcome (pair stays pending)
            Err(e) => out.send(Err(...))...            // per-side FileStat -> CompareOutcome Err side
        }
    }
}
```

`compare_one` (refactor of the existing fn): drop the `results: &Mutex<Vec<CompareOutcome>>`
param; take two `&mut Vec<u8>` buffers (fixes the current 2×4 MiB per-pair allocation —
a measured cost); return `Result<CompareOutcome>` where the outcome's `equal` is
`Result<bool, (FileId, FileStatError)>`. Keep the hardlink fast-path (same dev/inode →
`Ok(true)`), `compare_error_file_id`, and the guard "interrupt must not become an outcome".

`files_equal(a, b, shutdown, &mut buf_a, &mut buf_b)` — same compare logic, using the caller's
buffers, `check_in_flight()` per read chunk (aborts immediately on force).

### Constants (mirror hash)

```
const WORK_CAPACITY = 10_000;    // bounded work channel (ComparePair)
const OUT_CAPACITY  = 20_000;    // bounded result channel (CompareOutcome)
const FEED_CHUNK    = 1_024;     // rows pulled from list_pending_comparisons per round
const DRAIN_CHUNK   = 5_000;     // outcomes committed per transaction
```
Workers = `io_jobs` (`rt.config.process.io_jobs`) std::threads; `create_thread_bars` +
`thread_bar(0..io_jobs)` by value; `drop_thread_bars()` at phase end.

## DB additions / fixes (db/dedup.rs; facades in db.rs)

### Write (new)
- `create_temp_dedup_table(conn)` / `drop_temp_dedup_table(conn)` — fixed DDL above
  (also creates the TEMP `dedup_inflight`).
- `populate_temp_table(conn, eager)` — keep (idempotent `INSERT OR IGNORE`): `INSERT OR IGNORE
  INTO dedup_progress (sha1, size) SELECT sha1, size FROM files WHERE sha1 IS NOT NULL AND
  phase = '<prev>' GROUP BY sha1, size HAVING COUNT(*) > 1`.
- `ready_to_searching(conn, eager)` — ONE tx:
  (a) elect + flip: for each 'ready' group, set `canonical_id = id` on the `MIN(id)` electable
  member (`phase=prev`, `canonical_id IS NULL`, `(flags & :error_flag)=0`,
  `(flags & :check_flag)=0`, group in dedup_progress state 'ready'), then flip those groups to
  'searching';
  (b) **lone-canonical → done**: any group whose new canonical has no remaining pending
  candidate (`canonical_id IS NULL AND check=0`) gets its canonical promoted to `deduped`
  and `state='done'`.
- `list_pending_comparisons<R: SqlFileRow>(conn, eager, last_candidate_id: u64,
  limit: u64) -> Result<Vec<(R, R)>>` — see Query below. Returns `(candidate, canonical)`.
- `mark_inflight(conn, ids: &[FileId]) -> Result<()>` — `INSERT OR IGNORE INTO dedup_inflight
  (candidate_id) VALUES (:id)` per id (batched, one tx via caller/`with_transaction`).
- `unmark_inflight(conn, ids: &[FileId]) -> Result<()>` — `DELETE FROM dedup_inflight WHERE
  candidate_id = :id`.
- `count_pending_dedup_groups(conn) -> Result<u64>` — `SELECT COUNT(*) FROM dedup_progress
  WHERE state NOT IN ('done','errored')`.
- `count_dedup_phase_total(conn, eager) -> Result<u64>` — `SELECT COUNT(*) FROM files WHERE
  phase = '<prev>' AND (sha1, size) IN (SELECT sha1, size FROM files WHERE sha1 IS NOT NULL
  GROUP BY sha1, size HAVING COUNT(*) > 1)` (progress total; all of these end up `deduped`).
- `count_dedup_phase_done(conn, eager) -> Result<u64>` — same but `phase = 'deduped'`
  (progress position on resume).

### List query (the core "list function")

```sql
SELECT {R::sql_columns(Some("cand"))}, {R::sql_columns(Some("canon"))}
FROM files AS cand
JOIN dedup_progress AS dp ON dp.sha1 = cand.sha1 AND dp.size = cand.size AND dp.state = 'searching'
JOIN files AS canon ON canon.sha1 = cand.sha1 AND canon.size = cand.size
                   AND canon.canonical_id = canon.id AND canon.phase = '<prev>'
WHERE cand.phase = '<prev>'
  AND cand.canonical_id IS NULL
  AND (cand.flags & :check_flag) = 0                 -- error flag NOT excluded (user decision)
  AND cand.id > :last
  AND NOT EXISTS (SELECT 1 FROM dedup_inflight di WHERE di.candidate_id = cand.id)
ORDER BY cand.id
LIMIT :limit
```
Map with two `R::from_row(row, Some("cand"))` / `Some("canon"))` calls (see `db/place.rs` for
the dual-prefix pattern). `'searching'` guarantees exactly one joinable canonical per group.
The `NOT EXISTS` on `dedup_inflight` (≤ WORK_CAPACITY rows, PK-indexed) makes re-scans safe.

### Fix (existing WIP transitions — all have SQL/param bugs)

- `searching_to_finished` — verify: group has exactly 1 canonical and all non-canonical
  members are checked/errored; bind `:flag_completed` to `CheckWithCanonicalCompleted`.
- `finish_to_error` — params `:error_flag` → `ErrorWhileDedup`; groups where all
  non-canonical members errored; promote members to `deduped`, clear check flag.
- `finish_to_done` — currently binds `:error_flag` but SQL uses `:check_flags`; bind
  `:check_flag` to `CheckWithCanonicalCompleted`; no checked remain + has canonical; promote
  to `deduped`.
- `finish_to_ready` — currently binds `CheckWithCanonicalCompleted` to `:error_flag`;
  restructure: at least one checked remains AND at least one non-error remains + has canonical;
  unset check flags for the group; retire the canonical (`phase='deduped'` where
  `canonical_id = id` and group in 'ready').
- `ready_to_searching` — replaces the WIP (`(sha,size)` typo, missing `:check_flag` bind,
  split into the new atomic version above).
- `set_canonical` — keep: `UPDATE files SET canonical_id = :canonical_id, phase = 'deduped'
  WHERE id = :id`.
- `promote_errored_pending_to_deduped` — likely superseded by the transitions; remove.
- Remove legacy in-memory helpers no longer used: `pending_duplicate_groups`,
  `list_filtered_in_group`, `clear_check_with_canonical_completed`,
  `count_active_canonicals`, `promote_active_canonical_in_group`, `count_electable_pending`,
  `mark_active_canonical`. Keep `mark_self_canonical` / `promote_to_deduped` only if tests
  still reference them.

### Ingest chunk (archive/dedup.rs, in `db.with_transaction`)

For each outcome in chunk:
- `Ok(true)`  → `set_canonical(tx, candidate_id, canonical_id)`; `unmark_inflight(tx, [candidate_id])`; resolved += 1.
- `Ok(false)` → `set_file_flag(tx, candidate_id, CheckWithCanonicalCompleted, true)`; `unmark_inflight(tx, [candidate_id])`.
- `Err((failed_id, fse))` → `set_file_flag(tx, candidate_id, CheckWithCanonicalCompleted, true)`;
  `set_file_flag(tx, failed_id, ErrorWhileDedup, true)`;
  `unmark_inflight(tx, [candidate_id])`; `recorder.record_file(failed_id, Dedup, fse, default)`.
Then `progress.inc_both(resolved)` (per-promotion; matches "phase progress = files in dup
groups, advance on promote to deduped").

## Progress bar (settled)

- Global as usual (`inc_global`): bulk skips — `promote_non_ineligible_entries_to_dedup` +
  `promote_singleton_filtered_to_deduped` (before the machine).
- Phase bar: `set_phase_total(count_dedup_phase_total(eager))` (files in duplicate groups);
  `set_phase_position(count_dedup_phase_done(eager))` on resume (already-`deduped` members of
  those groups).
- `inc_both(n)` on every promotion to `deduped` (per-pair `set_canonical` = 1; bulk
  `finish_to_*` return the n rows they promoted). Note: error-promoted files and canonicals
  promoted retroactively are captured by the bulk-return counts; this may need a follow-up if
  the bar under/over-counts in the errored case (user: "not solidly covered at the moment").

## Interrupt / resume

- Single SIGINT = graceful: `check_between_files()` stops workers starting new pairs; in-flight
  compares finish; main drains+ingests; FSM skipped; `Err(Interrupted)`; state saved
  (`dedup_progress`, check/error flags, canonical ids persisted).
- Force (3× SIGINT): `check_in_flight()` inside `files_equal` aborts the read immediately; the
  worker sends no outcome; pair stays pending.
- Resume: `create_temp_dedup_table` + `populate_temp_table` (idempotent) + `ready_to_searching`
  (only touches 'ready' groups; persisted 'searching' groups resume as-is). `dedup_inflight`
  is a fresh TEMP table (empty) → candidates whose compare was lost are re-listed and
  re-compared; `set_phase_position(count_dedup_phase_done)` restores the bar.

## File-by-file change list

| File | Change |
|---|---|
| `db/dedup.rs` | rewrite: fixed DDL (+TEMP `dedup_inflight`), `ready_to_searching` (atomic elect+flip+lone→done), `list_pending_comparisons`, `mark_inflight`/`unmark_inflight`, `count_pending_dedup_groups`, `count_dedup_phase_total`/`_done`, fixed transitions; drop dead helpers |
| `db.rs` | facades for the above + keep `set_canonical`/`set_file_flag`/`count_files_in_phase` |
| `archive/dedup.rs` | crossbeam pipeline + per-group FSM loop; `compare_one`/`files_equal` buffer refactor; remove `prepare_round`/`load_pending_groups`/`establish_group_state`/`end_round`/rayon; keep `ComparePair`/`CompareOutcome`/`compare_error_file_id`/`warn_compare_pair_times`/`files_equal`/`sanity_check_flags` |
| `Cargo.toml` | no change needed (crossbeam-channel already a dep) |
| `plans/dedup-crossbeam.md` | this plan |

## Verification

1. `cargo build -p tar-dedup` + `-p tar-dedup-cli` (user dedup WIP may interfere; coordinate).
2. Correctness load: tree with heavy duplicates (reuse `/tmp/smoke/src` patterns):
   - dedup completes; `count_pending_dedup_groups()==0`; zero files left in prev_phase
     (`count_files_in_phase(FilePhase::Filtered|Hashed)` per eager/lazy).
   - canonical mapping consistent: every candidate has `canonical_id` set or is `deduped`; the
     2-member-unequal case lands in `done` (no hang).
   - errored-candidate case: a candidate with `ErrorWhileDedup` is still compared, never elected
     as a canonical.
3. Exactly-once under load: a slow 50 GiB group + many fast groups — fast groups advance FSM
   multiple rounds while the slow one grinds; no duplicated compares (watch worker count /
   total compares vs candidate count).
4. Interrupt: graceful (single) + force (3×) mid-dedup; resume completes; no double-compare.
5. Perf: `io_jobs` scaling, bounded RSS, no per-pair 8 MiB allocation (buffers reused).
6. Piped run: no control codes on stdout; log routing unchanged.

## As built (2026-09-23)

Implemented + functionally verified; the user still has to inspect the code before perf work.

### Deviations from the plan (all in `archive/dedup.rs`)

- **`fed` counter removed** — termination is `count_pending_dedup_groups()==0`, not
  fed/completed equality, so the counter was unused.
- **Flush buffer condition (BUG FOUND IN FIRST RUN):** the loop only applied outcomes when
  `pending_out.len() >= DRAIN_CHUNK` (5_000). For a run with fewer than DRAIN_CHUNK total
  compares, the outcomes sat unapplied forever while the loop spun (`busy=false` → 1 ms sleeps),
  leaving every group stuck in `searching` until SIGTERM. Fix mirrors the hash loop's
  `(feed_done && !pending_out.is_empty())` escape hatch:
  `if pending_out.len() >= DRAIN_CHUNK || (scan_exhausted && !pending_out.is_empty())`.
- **`compare_pair` bug fixed** (pre-existing): `candidate_device_id`/`candidate_inode_id`
  were copied from the **canonical**, which made the hardlink fast-path always fire (any pair
  "same inode"). Now use `candidate.device_id`/`candidate.inode_id`.
- `ComparePair` gained `candidate_size` so the per-worker `Bytes` bar has a length
  (`bar.set_length(pair.candidate_size)`).
- `ready_to_searching` returns `(elected, promoted)`; transitions return promotion counts so
  the phase bar is advanced exactly-once per file (per-pair `set_canonical` counts in the
  apply-chunk `resolved` return).

### Verification results (target tree much smaller than the plan's `/tmp/smoke` sizes)

All runs used a real-tree workflow: `archive --exit-after-stage hash`, then **python-injected
real SHA-1s + `phase='hashed'`** into the work DB (required because the user's in-progress
`ingest_hash_outcome` refactor at the time wrote nothing), then `resume --work-dir … --exit-after-stage dedup`.

1. Single-shot: 11-file tree (dup groups {3,2,2,2} + singleton + dir) → all `deduped`,
   canonical = min-id per group, 0 leftover in `hashed`/`filtered`, 0 `CheckWithCanonicalCompleted`
   (bit 128), `dedup_progress` dropped.
2. Resume: interrupted mid-run (timeout SIGTERM → graceful) left groups `searching` with all
   members resolved; resume re-anchored (`already_deduped=5`) and completed in ~30 ms.
3. 2-member unequal group (modified `b.txt` after hashing) → both files self-canonical
   (`canonical_id = id`), group `done`, no hang.
4. Errored candidate: pre-set `ErrorWhileDedup` (bit 256) on the min-id file → a different file
   was elected, the errored file was still compared and promoted to `deduped` (canonical_id set).
5. Graceful interrupt: "dedup stopped; completed compares saved saved=N" → resume completes.
6. Force abort (3× SIGINT mid-compare): **NOT completed.** 1 GiB compares finish in <0.4 s from
   page cache, faster than the signal loop; escalating to 8 GiB to widen the window was revoked
   to avoid disk pressure on a nearly-full `/tmp`. The force path shares
   `shutdown.check_in_flight()` inside `files_equal` with the already-verified hash phase; a
   lighter-weight trigger (slow tmpfs/FIFO-backed source) is a follow-up.

### Other notes

- The user's concurrent `hash.rs` refactor was mid-flight during verification (HashingOutcome →
  `db/hash.rs`, `ingest_hash_outcome` stub, `warn_if_times_changed` returning `bool` →
  `modified`). At last check it compiled but `ingest_hash_outcome` only writes when
  `hs.modified` (inverted — normal files got no sha1/phase); that's the user's WIP, not this
  phase.
- User also moved `meta::with_meta_txn` → `db/common.rs::with_transaction` mid-session; he
  completed the call-site rename himself.

## Follow-ups (recorded, NOT this round)

- Progress bar errored/canonical-promotion accounting if it proves off after load testing.
- Full dedup integration test suite.
- Shared `common` pipe module (hash + dedup + place) once the pattern settles.
- CLI knob for dedup batch sizes.

## Crossbeam API recap (crossbeam-channel 0.5.17)

`bounded::<T>(cap) -> (Sender<T>, Receiver<T>)`; `Sender::try_send(v)` (Ok / Full /
Disconnected), `send(v)` blocking; `Receiver::recv()` (Err(RecvError) when empty+disconnected),
`try_recv()`; NO `send_close`/`recv_iterator` — disconnection happens when all senders are
dropped. Workers loop `match work.recv() { Ok(pair) => …, Err(_) => break }`; main "closes"
the work channel with `drop(work_s)` when feeding ends, then definitively drains `out_r.recv()`
until `Err` (all worker senders dropped). Workers are `thread::Builder::new().name(..).spawn(
move || …)` with per-iteration owned bindings (`bar`, `shutdown.clone()`, channel clones,
`let ps = io_jobs`-style copies); `move` closures cannot borrow outer locals.