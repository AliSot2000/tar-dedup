# Plan: Dedup-phase unit tests (archive/dedup.rs + db/dedup.rs)

Status: **settled with the user** (2026-09-24). Scope: production bug-fixes (I1–I5) first,
then an inline unit-test suite for the dedup FSM/leaf functions and the phase `run()`. Tests
live in `#[cfg(test)] mod tests` at the bottom of the two source modules (private access to
`files_equal`, `compare_one`, `compare_worker`, `compare_error_file_id`, `record_dedup_error`,
`compare_pair`, `run_enqueue_dequeue_loop_dedup`, and all FSM fns).

## Current state of the code (verified 2026-09-24)

`Shutdown` (src/shutdown.rs) — NO `is_running()`:
- `is_interrupted() -> bool` = `mode == GRACEFUL || mode == FORCE` (i.e. "not running").
- `is_graceful() -> bool` = `mode == GRACEFUL`; `is_force() -> bool` = `mode == FORCE`.
- `check_between_files()` → `Err(Interrupted)` unless RUNNING; `check_in_flight()` →
  `Err(Interrupted)` only when FORCE.
- `request_graceful()`, `request_force()` (both pub). Tests: `graceful_request_stops_between_units_not_in_flight`,
  `force_request_aborts_everywhere`.

`archive/dedup.rs`:
- `run(rt)`:
  1. `promote_non_ineligible_entries_to_dedup(eager)` + `promote_singleton_filtered_to_deduped(eager)`
     (credits global progress), skips singletons/ineligible out of the phase.
  2. `create_temp_dedup_table()` + `populate_temp_table(eager)`; phase-var progress
     (`count_dedup_phase_total`/`..._position`); logs.
  3. early exit: `if count_pending_dedup_groups()? == 0 { sanity_check_flags(db)?; return Ok(()) }`.
  4. `jobs = config.process.io_jobs`; `detect_hardlinks = !config.indexing.no_hardlink_detection`.
  5. `thread_handles = Vec::<thread::JoinHandle<()>>::with_capacity(jobs)`; spawns `jobs`
     `compare_worker` threads via `thread::Builder::new().name("dedup-worker-{i}").spawn(...)`
     (collects the JoinHandles — DO NOT drop them), channels `bounded::<ComparePair>(WORK_CAPACITY=10_000)`
     and `bounded::<Option<CompareOutcome>>(OUT_CAPACITY=20_000)`.
  6. `let one_running = || at_least_one_running(&thread_handles.iter().collect());`
  7. `run_enqueue_dequeue_loop_dedup(&rt, work_s, out_r, one_running)? -> (fail_fast_hit, completed)`.
  8. `drop_thread_bars()`; `fail_fast_hit` → `Err(Error::Config("dedup fail-fast: …"))`.
  9. Tail: `match rt.shutdown.is_interrupted() { true => warn(saved = completed, …); Err(Error::Interrupted)
     false => { leftover = count_files_in_phase(prev_phase); if leftover != 0 panic!(…); sanity_check_flags; drop_temp_dedup_table; Ok(()) } }`.
     *(With the new `is_interrupted()` this already returns `Err(Interrupted)` for force too —
     no structural change needed; verified below as I2.)*
- `run_enqueue_dequeue_loop_dedup(rt, send, recv, one_running) -> Result<(bool, u64)>`:
  - state: `last_candidate: u64`, `feed_buf: Vec<(StrippedRecord, StrippedRecord)>`, `feed_i`,
    `feed_total`, `exited_threads`, `dequeued_total`, `completed`, `fail_fast_hit`,
    `pending_out: Vec<CompareOutcome>`, `busy`.
  - `apply_chunk |pending|` → `ingest_compare_outcome(&items)` (counts `resolved`), then
    `record_dedup_error(&mut recorder, item)` per item; returns `(n, resolved)`.
  - `drain_chunk |is_busy, exited, dequeue, override_drain|`: pulls `Some(outcome)` →
    push (dequeue+=1, busy=true), `Ok(None)` → `*exited += 1`, applies when `>= DRAIN_CHUNK ||
    override` (`completed += n`; `inc_both(resolved)`).
  - **Feed loop** (per iteration): `busy=false`; `if shutdown.is_interrupted() break;`
    `drain_chunk(false)`; FSM step block:
    `searching_to_finished(eager)`; `finish_to_error(eager)` → if `fail_fast && errored > 0`
    → tracing + `fail_fast_hit=true; break`; `finish_to_done`; `finish_to_ready`;
    `ready_to_searching`; `inc_both(promoted…)`; `if count_pending_dedup_groups() == 0 break;`
    feed: `if feed_i == feed_buf.len() { feed_buf = list_pending_comparisons::<StrippedRecord>(eager, last_candidate, FEED_CHUNK)?; feed_i=0; if feed_buf.is_empty() break; last_candidate = last.0.id.0; }`
    then `send.try_send(compare_pair(canon, cand))` per row (success → `mark_inflight` after
    the batch, via collected `sent: Vec<FileId>`); `if !busy sleep(1ms)`.
  - `drop(send)`.
  - **Drain tail**: loop { `if shutdown.is_interrupted() break;` `if dequeued_total == feed_total break;`
    `if exited_threads == io_jobs break;` `if !one_running() break;` `drain_chunk(…, true)`; `if !busy sleep(1ms)` }.
  - `drain_chunk(…, true)` once more; `recorder.flush()?; drop(recv)`; `Ok((fail_fast_hit, completed))`.
- `compare_worker(bar, shutdown, work: Receiver<ComparePair>, out: Sender<Option<CompareOutcome>>, detect_hardlinks)`:
  loop `work.recv()`: `Ok(pair)` → `if shutdown.check_between_files().is_err() break;`
  bar reset/length/message; `warn_compare_pair_times(&pair)`; `compare_one(...)`:
  `Ok(outcome)` → `out.send(Some(outcome)).expect("dedup worker: result channel closed")`;
  `Err(Interrupted)` → break; `Err(e)` → `panic!(…)`. `Err(_)` (recv) → break. After loop:
  `out.send(None).expect("dedup worker: result channel closed")`.
- `warn_compare_pair_times(pair) -> (bool, bool)` (canonical changed, candidate changed) via
  `warn_if_times_changed`.
- `compare_one(pair, shutdown, buf_a, buf_b, detect_hardlinks) -> Result<CompareOutcome>`:
  `shutdown.check_between_files()?`; `(cano, cand) = warn_compare_pair_times(pair)`;
  `pre_flight_check = if detect_hardlinks { (Some(oi),Some(od),Some(ai),Some(ad)) if oi==ai && od==ad => true, _ => false } else { false }`;
  `equal = match pre_flight_check { false => match files_equal(...) { Ok(v)=>Ok(v), Err(Interrupted)=>return Err(Interrupted), Err(e @ FileStat(_))=>Err(compare_error_file_id(pair,&e)), Err(e)=>panic!(…) }, true => Ok(true) };`
  `Ok(CompareOutcome { canonical_id, candidate_id, equal, canonical_modified: cano, candidate_modified: cand })`.
- `compare_error_file_id(pair, e) -> (FileId, FileStatError)`: `path = e.io_path().expect("compare produced a non-Io…")`;
  `if path == pair.canonical_path { canonical_id } else if path == pair.candidate_path { candidate_id } else { panic!(…) }`;
  `(file_id, e.to_only_file_stat().expect("PRECONDITION FAILED. FileStatError only…"))`.
- `record_dedup_error(recorder, e: &CompareOutcome)`: `match &e.equal { Err((file_id, error)) =>
  recorder.record_file(*file_id, ErrorPhase::Pipeline(PipelinePhase::Dedup), error.recreate(), ErrorFlags::default()),
  Ok(_) => return }`.
- `files_equal(a, b, shutdown, buf_a, buf_b) -> Result<bool>`: `File::open` both (map_err
  `Error::io`) → `metadata().len()` both → `if len_a != len_b return Ok(false)` → loop {
  `shutdown.check_in_flight()?`; `fa.read(buf_a)`, `fb.read(buf_b)` (map_err `Error::io`);
  `if na==0 && nb==0 return Ok(true)`; `if na != nb || buf_a[..na] != buf_b[..nb] return Ok(false)` }.

`db/dedup.rs`:
- `CompareOutcome { canonical_id: FileId, candidate_id: FileId, equal: Result<bool, (FileId, FileStatError)>, canonical_modified: bool, candidate_modified: bool }`.
- `ComparePair` (`#[derive(Clone)]`): canonical_*/candidate_* (id, path, mtime/atime/ctime,
  device_id, inode_id) + `candidate_size: u64`.
- `compare_pair(canonical: &StrippedRecord, candidate: &StrippedRecord) -> ComparePair`.
- `prev_phase(eager) -> &str` = `"hashed"` if eager else `"filtered"`.
- `dedup_progress (sha1 BLOB, size, state 'ready'|'searching'|'finished'|'errored'|'done' default 'ready', PK(sha1,size))`;
  `dedup_inflight (candidate_id PK)` — real TEMP table (per connection).
- FSM (all guarded, single tx each, `phase = <prev>`):
  - `searching_to_finished`: `state='searching'` groups where in-phase members have
    `SUM(canonical_id IS NOT NULL) = 1` AND `SUM(resolved: check-flag OR canonical) = COUNT(*)`. **(I4: change to `canonical_id = id`)**
  - `finish_to_error`: `state='finished'` groups where non-canonical members ALL carry
    `ErrorWhileDedup` (`SUM(error) = COUNT(*)-1`) AND `SUM(error) > 0` AND `SUM(canonical) = 1`
    **(I4: `canonical_id = id`)** → sets `errored`; then promotes ALL group files to
    `phase='deduped'` and clears the check flag.
  - `finish_to_done`: `state='finished'`, no checked members (`SUM(check) = 0`), `SUM(canonical)=1`
    **(I4: `= id`)** → `done`; promotes group files to `deduped`, clears check flags.
  - `finish_to_ready`: `state='finished'`, `SUM(check) > 0` AND `SUM(error-free) > 0` AND
    `SUM(canonical)=1` **(I4: `= id`)** → `ready`; clears the group's check flags; retires the
    current canonical (`phase='<prev>' AND canonical_id = id` in ready groups) → `deduped`.
  - `ready_to_searching`: (a) elect: `UPDATE files SET canonical_id = id WHERE id IN (SELECT MIN(id) … WHERE phase='<prev>' AND canonical_id IS NULL AND (flags & error) = 0 AND (flags & check) = 0 AND group ready GROUP BY sha1,size)`; (b) flip `ready → searching` where in-phase `SUM(canonical_id = id) = 1` AND `SUM(NULL-canonical AND no-check) > 0`; (c) promote lone selves: `UPDATE files SET phase='deduped' WHERE phase='<prev>' AND canonical_id = id AND group ready`; (d) **blanket `UPDATE dedup_progress SET state='done' WHERE state='ready'` — I5: guard on "no in-phase members left"**.
- `list_pending_comparisons<R: SqlFileRow>(conn, eager, last_candidate_id, limit) -> Vec<(R candidate, R canonical)>`:
  joins `files cand` + `dedup_progress dp (state='searching')` + `files canon (canonical_id = canon.id, phase='<prev>')`,
  excludes in-flight (`NOT EXISTS dedup_inflight`), requires `cand.phase='<prev>' AND cand.canonical_id IS NULL AND (cand.flags & check)=0 AND cand.id > :last ORDER BY cand.id LIMIT :limit`.
- `mark_inflight(conn, ids)` / `unmark_inflight(conn, ids)` — INSERT OR IGNORE / DELETE on dedup_inflight.
- `set_canonical(conn, file_id, canonical_id)`: `canonical_id = :canonical_id, phase = 'deduped' WHERE id = :id`.
- `mark_self_canonical(conn, file_id)`: `canonical_id = id, phase = 'deduped' WHERE id = :id`.
- `promote_non_ineligible_entries_to_dedup(conn, eager)`: `phase='<prev>' AND (ftype != 'file' OR sha1 IS NULL OR (flags&sha_err)!=0 OR NOT (generate_archive_filter))` → `deduped`.
- `promote_singleton_filtered_to_deduped(conn, eager)`: unique `(sha1,size)` in `<prev>` → `deduped`.
- `count_check_with_canonical_completed(conn)`, `count_pending_dedup_groups(conn)`,
  `count_dedup_phase_total/…_position(conn, eager)`, `dedup_workload_where(eager)`.
- `ingest_compare_outcome(conn: &mut Connection, results: &Vec<CompareOutcome>) -> Result<u64>`:
  one tx; per outcome: set `Modified` on canonical/candidate if the respective `*_modified`;
  `match &equal { Ok(true) => set_canonical(candidate, canonical), resolved+=1;
  Ok(false) => set_file_flag(candidate, CheckWithCanonicalCompleted, true) (assert rows==1);
  Err((failed_id,error)) => set check flag on candidate (assert 1) AND set ErrorWhileDedup on failed_id (assert 1) }`;
  `tx.commit()?; Ok(resolved)`. **I1: also unmark each outcome's candidate_id from dedup_inflight in this tx.**

`db.rs` facades (all present): `promote_singleton_filtered_to_deduped`, `promote_non_ineligible_entries_to_dedup`,
`create_temp_dedup_table`, `drop_temp_dedup_table`, `populate_temp_table`, `count_pending_dedup_groups`,
`count_dedup_phase_total`, `count_dedup_phase_position`, `searching_to_finished`, `finish_to_error`,
`finish_to_done`, `finish_to_ready`, `ready_to_searching`, `list_pending_comparisons::<R>`,
`mark_inflight`, `unmark_inflight`, `set_canonical`, `mark_self_canonical`, `count_check_with_canonical_completed`,
`ingest_compare_outcome`. `pub(crate) mod dedup` in `db.rs`.

`common.rs`: `at_least_one_running(threads: &Vec<&thread::JoinHandle<()>>) -> bool` (true if any
`!h.is_finished()`).

Flag bits (db/flags.rs): `CheckWithCanonicalCompleted = 7`, `ErrorWhileDedup = 8`, `Modified = 5`,
`ErrorWhileHash = 6`, `FileHardlinkCanonical = 1`.

## Production fixes (apply FIRST, each minimal & surgical)

### I1 — `unmark_inflight` must be called (multi-round groups wedge)
`archive/dedup.rs::apply_chunk` only calls `ingest_compare_outcome`; nobody deletes the
`dedup_inflight` rows. A group needing a second compare round (e.g. {A,B,C} with A≠B and A≠C)
reaches `ready → searching` round 2, but the leftover candidates (C) are still excluded by
`NOT EXISTS dedup_inflight` → `list_pending_comparisons` returns ∅ → feed loop `break`s →
`dequeued == feed_total` → tail exits → `run()` success-path `count_files_in_phase(prev) != 0`
→ **panic "dedup finished with N file(s) still in …"**.
Fix in **`db/dedup.rs::ingest_compare_outcome`** (not archive/dedup.rs — per user), inside the
existing tx, per outcome: `unmark_inflight(&tx, &[outcome.candidate_id])` before/after the
match arm (call the existing helper; it takes `&Connection`, and `&tx` derefs to one).
Entry should `dedup_inflight` be present? The marker may already be gone (resume cleared TEMP,
or a past run); DELETE is idempotent — fine.

### I2 — force now handled by `is_interrupted()` (no code change, verify by test)
With `is_interrupted()` = graceful OR force, both loops (`feed` + `drain tail`) already break on
force, the tail `match` already returns `Err(Interrupted)` for force, and the leftover panic is
only reachable on the success (running) arm. Leave code untouched; the force tests pin this.
Resume safety: dedup tables are only dropped on success → force resume reuses `dedup_progress`.

### I3 — join worker handles before `drop(recv)` (drop-race → worker panic)
Today the drain tail's breaks happened for interrupt and `dequeued == feed_total` *before* a
worker finished its in-flight pair / sent its final `None`, so `compare_worker`'s
`out.send(...).expect("dedup worker: result channel closed")` panicked after `drop(recv)`.
Fix in **`archive/dedup.rs`**:
1. Thread the handles into the loop fn: change
   `fn run_enqueue_dequeue_loop_dedup(rt, send, recv, one_running: impl Fn() -> bool)`
   to also take `handles: &Vec<thread::JoinHandle<()>>`; pass `&thread_handles` from `run()`.
2. In the drain tail, replace the final
   `drain_chunk(...)?; recorder.flush()?; drop(recv);`
   with:
   ```
   // Close out the worker threads before dropping the channel: each worker
   // sends its last outcome/None *before* it returns, so joining every handle
   // guarantees no `out.send` can hit a dropped receiver (panics on
   // "result channel closed"). `join` also honours "finish in-flight" on a
   // graceful stop (workers exit after their current pair resolves).
   for handle in handles {
       let _ = handle.join();
   }
   drain_chunk(&mut busy, &mut exited_threads, &mut dequeued_total, true)?;
   recorder.flush()?;
   drop(recv);
   ```
   Join exactly once per handle (JoinHandle is not Clone; join is the owned permission).
   Ignore the `Result` (a panicked worker returns `Err` — record nothing special).
   A worker stuck in a FIFO read would block the join — acceptable, same as any pipeline.
3. Optional (keeps the prompt non-interrupt path): the `dequeued == feed_total` and
   `exited_threads == jobs` and `!one_running()` breaks remain as-is; join is the safety net.

### I4 — encode "exactly one self-canonical in `<prev>`" in the FSM guards
Replace `canonical_id IS NOT NULL` with `canonical_id = id` (the `files` alias-less subquery —
in these guard subqueries the column refers to the same `files` row) in **the 4 transition
guards** (`db/dedup.rs::searching_to_finished`, `finish_to_error`, `finish_to_done`,
`finish_to_ready`). `ready_to_searching`'s `searching` flip already uses `canonical_id = id`.
Semantics unchanged (in-phase rows with a canonical are always the active self-canonical), but
the SQL now states the invariant the user described. Double-check each `SUM(CASE WHEN
canonical_id = id THEN 1 ELSE 0 END) = 1` against the surrounding group-by query.

### I5 — guard the blanket `ready → done` flip (keep it, don't drop)
The statement `UPDATE dedup_progress SET state = 'done' WHERE state = 'ready'`
(`ready_to_searching`, step d) is **required** for lone-canonical groups (2-member-unequal:
after `finish_to_ready` retires canon A and `ready_to_searching` elects B, B is the lone in-phase
member, so the `searching` flip guard `SUM(pending) > 0` fails; the blanket flip is the only
thing that closes the group — remove it and the phase never terminates). Its only flaw is the
unreachable no-electable-member edge. Guard it so it only fires when the group really has no
in-phase members left (after the lone-canonical promote at step c ran):
```
UPDATE dedup_progress SET state = 'done'
WHERE state = 'ready'
  AND (sha1, size) NOT IN (SELECT sha1, size FROM files WHERE phase = '<prev>')
```
(uses the function's existing `phase` local). This is order-safe: step (c) already promoted the
lone self-canonical to `deduped` before step (d) runs in the same tx.

## Test harness

### A. `db/dedup.rs` tests (`#[cfg(test)] mod tests`)
Imports: `use super::*;` (gives `Connection`, `named_params`, all fns, `FileFlag`,
`StrippedRecord`, `FileId`, …), `use crate::db::schema;`, `use crate::error::ToPanic;`,
`use rusqlite::named_params;` (already at top). Helpers:
- `fn open_db() -> (tempfile::TempDir, Connection)` — `Connection::open(dir.path().join("t.sqlite"))`
  + `schema::initialize(&conn)`.
- `fn seed_file(conn, id, abs_path, sha1_byte: u8[, size, phase, flags, canonical_id])` — raw
  `INSERT INTO files (id, abs_path, ext, size, ftype, phase, sha1, include_reason_archive, exclude_reason_archive, flags, canonical_id, dev, inode)`
  (`ftype='file'`, `include_reason_archive=-1`, `exclude_reason_archive=0`, defaults size = sha1 byte
  or explicit). Group identity: same `(sha1, size)` for members; use `(sha1_byte, size)` pairs.
- `fn seed_group(conn, sha1_byte, size[, state])` — raw INSERT/UPDATE into `dedup_progress`
  (or use `create_temp_dedup_table` + `populate_temp_table` where convenient).
- Per test: `create_temp_dedup_table(&conn)` before seeding `dedup_progress`, unless a test
  specifically verifies populate.

For eager-variant FSM behavior: one helper param or a second lightweight helper that seeds
`phase='hashed'` and passes `eager_filter: true` (transition fns take `eager_filter`).
Default tests use non-eager (`phase='filtered'`, `eager_filter: false`).

### B. `archive/dedup.rs` tests (`#[cfg(test)] mod tests`)
Mirror the hash `TestWorld` (see `archive/hash.rs` bottom for the pattern to copy): tempdir,
real files, `ArchiveConfig` hand-literal, `ProgressBarSet::new(ARCHIVE_MULTIPLIER)`,
`Shutdown::detached()`, `rt() -> ArchiveRTArgs<'_>`.
- `struct TestWorld { dir, db, shutdown, progress, config }`, methods `path(&str) -> PathBuf`,
  `add_file(name, payload) -> FileId` (write real file + `insert_file` with `ftype=Some(FileType::File)`,
  dev/inode `None` — the preflight requires None for the byte-compare path), then
  **after all inserts**: `db.apply_no_filter()` (sets `phase='filtered'`, `include_reason_archive=-1` —
  exactly the non-eager dedup precondition), then seed sha1 per file via
  `db.with_transaction(|conn| conn.execute("UPDATE files SET sha1 = :sha1 WHERE id = :id", named_params!{...}))?`.
  Helper `seed_sha1(id, sha1_byte)` + optional `seed_flag(id, FileFlag, on)` +
  `mark_self_canonical_phase(id)` via SQL (`canonical_id = id, phase='deduped'`).
- `test_archive_config() -> ArchiveConfig`: copy the hash test's config but with
  `indexing.no_hardlink_detection: true` (default for these tests → byte-compares; a dedicated
  preflight test overrides in-memory), `process.jobs`/`process.io_jobs` small (e.g. 2), and for
  `run()` calls `process.fail_fast` as needed. `filter.eager_filter: false` default.
- For eager: `helper seed_eager()` → after apply_no_filter, `UPDATE files SET phase='hashed'`;
  config `eager_filter: true`.
- Worker spawn helper (like hash's `run_loop`): build both channels, `create_thread_bars(BarKind::Bytes, jobs)`,
  `thread_bar(i)` clones, spawn `compare_worker`, keep the `JoinHandle`s, drop the parent-side
  clones, call the loop fn or `run()`.

File content helpers: `pattern(n, seed)` + `sha1_of(payload)` (copy from hash tests — private,
so duplicate them in this module).

## Test inventory

### db/dedup.rs — FSM + helpers (raw Connection)

1. `ready_to_searching_elects_min_and_flips`
   Group {1,2,3} seeded `phase='filtered'`, all `sha1=7, size=100`, no canonical, group `ready`.
   Call `ready_to_searching(&conn, false)`. Assert: returns `(elected→1, promoted→0)`; file 1 has
   `canonical_id = id`; group state `searching`; 2,3 untouched.
2. `ready_to_searching_two_member_group`
   {1,2}, 1 already `mark_self_canonical_phase` (canonical_id=1, phase='deduped'), 2 in `filtered`
   unchecked, group `ready`. Call → elect 2 (the only NULL-canonical), searching flip needs a
   pending member → none (2 is now canonical) → **lone path**: 2 promoted `deduped`
   (`promoted →1`), group `done`. Assert both files `canonical_id IS NOT NULL`.
3. `ready_to_searching_no_electable_marks_done_unreachable_guard` **(I5 guard)**
   Group {1,2} both `filtered`, both with `ErrorWhileDedup` set, group `ready`. Call → election
   elects nothing; searching flip no; lone-promote no; **guarded done-flip**: since in-phase
   files still exist (2 of them), the group must REMAIN `ready` (and `count_pending` stays >0).
   *This documents the I5 guard; before the guard the blanket flip would have set `done`.*
4. `searching_to_finished_fires_when_all_resolved`
   {1 canonical=self, 2 checked(`CheckWithCanonicalCompleted`), 3 checked}, group `searching`.
   Call → 1 group transitions `finished`. Change: 3 unchecked → no transition (pending remains).
5. `searching_to_finished_requires_exactly_one_self_canonical`
   {1,2 both `canonical_id=id` in `filtered`, 3 checked}, group `searching` → no transition
   (guard `SUM(canonical_id = id) = 1` fails). Also 0 canonicals → no transition.
6. `finish_to_done_promotes_and_all_have_canonical`
   Group `finished`: canonical 1 in `filtered` (self), candidates 2,3 both `deduped` with
   `canonical_id = 1` (set via `set_canonical`, simulating equal compares). Call → group `done`;
   files promoted: 1 → `deduped`; all three files `canonical_id IS NOT NULL`; check flags cleared.
   Negative: a member still in `filtered` with check flag → no transition.
7. `finish_to_error_errored_with_all_errored_candidates`
   Group `finished`: canonical 1 self, candidates 2,3 in `filtered` both `ErrorWhileDedup` +
   `CheckWithCanonicalCompleted`. Call → `(errored→1, promoted→?)`; group `errored`; 2,3 stay
   `canonical_id NULL` + `ErrorWhileDedup` sticks, check flag cleared; phase `deduped`.
   Negative: candidate without error flag → no transition.
8. `finish_to_ready_retires_canonical_and_clears_flags`
   Group `finished`: canonical 1 self, candidates 2,3 in `filtered` both checked, 2 error-free,
   3 with `ErrorWhileDedup`. Call → group `ready`; flags cleared for the group; canon 1 promoted
   `deduped`; 2,3 remain `filtered`. Negative: no checked members → no transition (→ done path).
9. `fsm_roundtrip_two_member_unequal_terminates_done`
   Drive {1,2} through the fn sequence in the loop order until `count_pending == 0`:
   `ready_to_searching` → elect 1, `searching`; 2 pending.
   (Simulate compare result) → `ingest_compare_outcome` with `Ok(false)` on 2 →
   `searching_to_finished` → `finish_to_ready` → `ready_to_searching` (lone: 2 elected, promoted,
   group done). Assert final: group `done`, 1 `deduped` self, 2 `deduped` self, `count_pending==0`.
10. `fsm_roundtrip_three_member_unequal_than_equal_terminates_done` **(I1 repro, db level)**
    {1 self canonical, 2,3 in filtered unchecked}, group `searching`.
    `ingest_compare_outcome` with `Ok(false)` for 2 and 3 → `searching_to_finished` →
    `finish_to_ready` → `ready_to_searching` (2 elected self, group `searching`).
    **Assert `list_pending_comparisons` still lists candidate 3** (this fails today — the stale
    `dedup_inflight` row for 3 would exclude it; with the I1 fix + `mark_inflight`&`unmark_inflight`
    called around the real loop simulation it passes). Simulate compare 3 vs 2 equal →
    `ingest_compare_outcome` Ok(true) → `searching_to_finished` → `finish_to_done`.
    Assert: group `done`, 3 `canonical_id = 2`, 2 `canonical_id = 2`, 1 `canonical_id = 1`,
    all `deduped`, `count_pending == 0`.
11. `ingest_compare_outcome_true_links_and_resolves`
    candidates 2 vs canonical 1 → call `ingest_compare_outcome` with `Ok(true)` → 2 gets
    `canonical_id=1`, `phase='deduped'`; returns resolved 1; **and candidate 2 is unmarked from
    `dedup_inflight`** (seed an inflight row first; assert empty after).
12. `ingest_compare_outcome_false_sets_check_flag`
    `Ok(false)` → candidate gets `CheckWithCanonicalCompleted`; resolved 0; inflight cleared.
13. `ingest_compare_outcome_error_sets_flags_on_failed_side`
    `Err((FileId(failed), FileStatError::Io{...}))` → candidate gets check flag, failed id gets
    `ErrorWhileDedup`, inflight cleared; resolved 0. (failed may be canonical or candidate.)
14. `ingest_compare_outcome_sets_modified_flags`
    outgoing with `canonical_modified=true`, `candidate_modified=true` → both rows get
    `Modified`.
15. `list_pending_comparisons_slices_and_orders`
    group `searching` with canonical 1 + candidates 2,3,4 → list with `last_candidate_id=0,
    limit=2` → rows [(2,1),(3,1)]; next slice `last=2` → [(4,1)]; then empty.
16. `mark_unmark_inflight_excludes_and_restores`
    `mark_inflight(&[2,3])` → list returns only 4; `unmark_inflight(&[3])` → 3 returns, 2 still
    hidden; `mark_inflight(&[2])` again (idempotent) → still hidden.
17. `list_pending_comparisons_empty_while_pending_groups_exist`
    group `searching`, the only candidate 2 marked inflight → list returns ∅ while
    `count_pending_dedup_groups() > 0` (documents: in-flight work is owned by the drain tail).
18. `promote_non_ineligible_and_singleton`
    `phase='filtered'` rows: (a) singleton `(sha1,size)` → `promote_singleton_filtered_to_deduped`
    → `deduped`; (b) `ftype != 'file'`/`sha1 IS NULL`/`flags&ErrorWhileHash`/`include_reason_archive = 0`
    → `promote_non_ineligible_entries_to_dedup` → `deduped`; (c) a real duplicate group member
    (correct sha1, incl=-1) stays `filtered`.
19. `count_pending_dedup_groups_tracks_states`
    seed one each of ready/searching/finished + one done + one errored → `count_pending == 3`
    (done/errored excluded).

### archive/dedup.rs — leaf & worker (TestWorld A)

20. `files_equal_same_content` — two equal 1 MiB files → `Ok(true)`.
21. `files_equal_diff_length` — 1 MiB vs 1 MiB+1 → `Ok(false)` (length gate, no read).
22. `files_equal_diff_content_same_length` → `Ok(false)`.
23. `files_equal_force_pre_set_interrupts` — `world.shutdown.request_force()`; equal files →
    `Err(Interrupted)` (first `check_in_flight` after open). Assert matches `Error::Interrupted`.
24. `files_equal_open_permission_denied` — euid guard: `use nix::unistd::geteuid; if geteuid().is_root() { return; }`;
    candidate `0o000` (via `std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0))`)
    → `Err` matching `Error::FileStat(_)` and the inner `FileStatError::Io` (or at least kind
    `Io/PermissionDenied`).
25. `compare_error_file_id_canonical_side`
    `compare_error_file_id(&pair, Error::FileStat(FileStatError::Io{path: canonical_path, source: io::Error::new(PermissionDenied, …)}))`
    → `(pair.canonical_id, fse)` with `fse.io_path() == canonical_path`.
26. `compare_error_file_id_candidate_side` — same with candidate path → `(pair.candidate_id, …)`.
27. `compare_error_file_id_unknown_path_panics` — `Error::FileStat(Io{path: OTHER})` →
    `std::panic::catch_unwind(AssertUnwindSafe(|| { let _ = compare_error_file_id(&pair, &e); Ok(()) })).is_err()`
    **(Q4 panic assert)**.
28. `record_dedup_error_only_records_errors`
    `Recorder::new(&world.db, true)`; `record_dedup_error(&rec, &outcome_ok_true)` and `..._ok_false`
    → recorder empty after flush; `record_dedup_error(&rec, &outcome_err(fid))` →
    `recorder.flush()` → exactly one errors row with `file_id == fid`, `phase ==
    ErrorPhase::Pipeline(PipelinePhase::Dedup)`.
29. `compare_one_byte_compare_equal` — hardlink fields `None` (the default world seed), equal
    files, `Shutdown::detached()` → `Ok(CompareOutcome{ equal: Ok(true) })`, both modified false.
30. `compare_one_byte_compare_unequal` → `equal: Ok(false)`.
31. `compare_one_hardlink_preflight_short_circuits` — `detect_hardlinks = true`; build a real
    hard-link pair (`std::fs::hard_link(&a, &b)`, see unarchive/place.rs for the call shape) so
    both paths resolve to the same `(dev, inode)`; seed one path with different content (hardlink
    guarantees identical content anyway) → `equal: Ok(true)` **without reading** (assert via the
    equal outcome despite a differing... same inode ⇒ same content; the point is the preflight
    path is taken — also assert a **hand-built ComparePair** whose paths point at *different
    content* files but share `(device_id, inode_id)` → still `Ok(true)`).
32. `compare_one_times_modified_detected` — build pair with stale recorded mtime (now−1h, see
    hash `modified_file_flagged`) on candidate → `candidate_modified == true` in the outcome.
33. `compare_one_graceful_pre_set_interrupts` — `request_graceful()` → `Err(Interrupted)` at the
    `check_between_files` gate (no outcome).
34. `compare_one_force_pre_set_interrupts` — `request_force()` → `Err(Interrupted)`.
35. `compare_one_io_error_mapped_to_file_id` — candidate `0o000` (euid guard), equal-length
    canonical file → `Ok(CompareOutcome{ equal: Err((candidate_id, FileStatError::Io)) })` with
    the path matching candidate.
36. `compare_pair_maps_records` — two `StrippedRecord`s → assert every field maps
    (ids, paths, size, times, dev/inode).
37. `compare_worker_sends_none_on_graceful_exit`
    Spawn `compare_worker` with 1 work item pre-marked in a channel, `request_graceful()` set
    before the pair is pulled → worker breaks at `check_between_files` → `out_r.recv()` ==
    `Ok(None)` and no `Some` ever (use `try_recv`×N).
38. `compare_worker_sends_none_after_force_mid_compare`
    1 worker, a large pair (16 MiB equal files), trigger thread polling
    `world.progress.thread_bar(0).position() > 0` then `request_force()` → worker aborts the
    in-flight read → no `Some` for the pair → `out_r.recv() == Ok(None)` (bounded by a
    `recv_timeout` / poll); then the JoinHandle can be `join()`ed. This also pins the drop-race
    fix (I3): the None must arrive **before** the drain tail drops recv.

### archive/dedup.rs — phase `run(&rt)` tests

39. `run_nothing_to_do_exits_early` — only singleton files (unique sha1s) → `run` returns `Ok(())`
    without spawning workers; all files `phase='deduped'`; `count_pending_dedup_groups() == 0`;
    no panic. (Also assert `sanity_check_flags` passes with no phantom flags.)
40. `run_basic_equal_and_unequal_groups`
    Two groups: {3 equal files} and {2 unequal files} (equal-length different content). All seeded
    `filtered` + sha1. `run` → `Ok`; every file `phase='deduped'`; equal group: canonical = min
    id, children `canonical_id = min`; unequal group: both `canonical_id = id` (self); no leftover.
41. `run_multiround_group_completes` **(I1 end-to-end repro)**
    Group {A,B,C}: A vs B and A vs C differ, B == C. `run` → `Ok`; all `deduped`; C (or C&[2nd
    round canonical]) linked to B; no "still in filtered" panic; `count_check_with_canonical_completed == 0`.
    *(Red before I1 — documents the fix.)*
42. `run_fail_fast_on_errored_group` — {A ok, B `0o000` + pre-flagged `ErrorWhileDedup`},
    `process.fail_fast = true` → `run` returns `Err(Error::Config)` containing "fail-fast"; the
    errored group marked `errored`; with `fail_fast = false` → `run` `Ok`, group `errored`, files
    promoted `deduped`, B keeps `ErrorWhileDedup` + own payload (`canonical_id NULL`).
43. `run_graceful_interrupt_mid_run_resume_completes`
    1 worker, several groups (incl. an 8 MiB pair to widen the window for the trigger thread
    polling `thread_bar(0)` `pos(0→>0→0)`, then `request_graceful()`), `run` → `Err(Interrupted)`;
    `dedup_progress` still exists (`count_pending_dedup_groups() > 0`), completed compares saved;
    fresh `Shutdown::detached()` + `run` again → `Ok`, all files `deduped`.
44. `run_force_interrupt_mid_run_falls_through_interrupted` **(I2 pin)**
    force via trigger on `pos > 0` → `run` returns `Err(Interrupted)` (was: panic on leftover —
    red before the Shutdown change), `dedup_progress` survives, resume completes.
45. `run_interrupt_before_start_returns_interrupted` — `request_graceful()` / `request_force()`
    set before `run` → `Err(Interrupted)`; no file moved; tables intact.
46. `run_eager_parametrized` **(Q2)** — rerun the `run_basic_equal_and_unequal_groups` scenario
    with `eager_filter = true` and files seeded `phase='hashed'` (helper). Shared setup fn, two
    thin tests.
47. `loop_exit_via_dequeued_eq_feed_total` **(I3/I3ajoins)**
    Drive `run_enqueue_dequeue_loop_dedup` directly (like the hash `run_loop` helper): 1 worker,
    one group with a single pair, run the loop → loop returns `(false, 1)`; worker joined; no
    panic; candidate linked.
48. `loop_exit_via_exited_threads_and_one_running`
    Pre-`request_graceful()` before feeding (workers break at first `check_between_files`, send
    `None`); call the loop fn → still terminates (`exited_threads == io_jobs` and/or
    `!one_running()` (JoinHandle `is_finished`)); all outcomes drained; `Ok((false, 0))`.

## Idioms & gotchas

- `thread::JoinHandle<()>` is `Send`+`Sync` but **not `Clone`**; join exactly once per handle at
  the end. Do not drop `thread_handles` early (drop = detach).
- `Schema::initialize` is `crate::db::schema::initialize(&conn)`; use raw `Connection` for the
  db-side tests (single active statement per connection; pull/apply sequentially).
- `dedup_inflight` is a TEMP table — recreated per connection; db tests that exercise
  inflight must `create_temp_dedup_table` on the same `Connection` they list with.
- Seeding `files` for the archive `run` tests: `insert_file` → `apply_no_filter` (idempotent;
  sets `include_reason_archive=-1`, `phase='filtered'`) → raw sha1 UPDATE. Order matters:
  apply_no_filter sets phase, then sha1, before `run`.
- Non-eager dedup's prev phase is `filtered`; eager is `hashed` — `prev_phase(eager)`.
- euid guard for `0o000` tests: `use nix::unistd::geteuid; if geteuid().is_root() { return; }`.
- `catch_unwind(AssertUnwindSafe(|| -> Result<()> { let _ = <panicking call>?; Ok(()) })).is_err()`
  — dev profile only (release is `panic="abort"`); never `--release` for tests.
- `assert_eq!`/`assert_ne!`/`matches!`/`#[test]`/`#[ignore]` as in the hash tests.
- Trigger threads: poll `world.progress.thread_bar(0).position()` (clones share state via Arc)
  every 1 ms; `join()` the trigger before asserting; fall back to a bounded poll cap so a starved
  trigger can't hang the suite.
- `fs::hard_link(&src, &dst)` for real hardlink pairs (see `unarchive/place.rs:303`).
- `io_buffer()`/`IO_BUF_SIZE` come from `crate::common`; the worker buffers are 4 MiB each.

## Build / test commands

- `timeout 900 nix develop --command cargo build -p tar-dedup -p tar-dedup-cli` — prod green, zero
  new warnings in the touched files (hash/dedup/shutdown carry pre-existing warnings; keep dedup
  additions warning-free).
- `timeout 1500 nix develop --command cargo test -p tar-dedup --lib` — full lib suite; expect
  only the 4 pre-existing failures (`db::inventory::tests::{major_minor_null_for_regular_file, two_sources_share_one_file_row}`,
  `unarchive::scan::tests::{scan_caches_payloads_and_promotes_on_snapshot, scan_interrupt_persists_state_and_resume_skips_processed_members}`).
- Targeted: `cargo test -p tar-dedup --lib -- dedup` (filter by test-name substring), and the
  new `db::dedup::tests::*` / `archive::dedup::tests::*` names for focused iteration.

## As-built checklist (filled in after implementation)

- [x] I1 unmark-in-ingest in place; multi-round run() test green end-to-end (`run_multiround_group_completes` + db-level `fsm_roundtrip_three_member_unequal_than_equal_terminates_done`).
- [x] I2 force→Interrupted pinned (run-level `run_interrupt_before_start_returns_interrupted`; the drain-tail/feed loops break on `is_interrupted()` = both modes).
- [x] I3 join-handles tail in place; no `result channel closed` panics across the suite. (`run_enqueue_dequeue_loop_dedup` now owns the handles: `mut Vec<JoinHandle<()>>`, joins each via `take` after the drain tail, before the final `drain_chunk` + `drop(recv)`. The `one_running` closure was dropped in favor of inline `at_least_one_running(&handles.iter().collect())` because `JoinHandle::join` consumes.)
- [x] I4 guards use `canonical_id = id`; FSM guard tests green (`searching_to_finished_requires_exactly_one_self_canonical` two/zero canonical negatives).
- [x] I5 guarded `ready→done`; `ready_to_searching_no_electable_marks_done_unreachable_guard` documents it.
- [x] eager parametrized variant green (`run_eager_parametrized`).
- [ ] Hash test-suite re-sync (follow-up, user's rewrite): rerun hash tests against the None/`exited_threads`/liveness loop and fix any test that targeted the old loop shape or shutdown semantics.

### Fixes discovered while writing the tests (deviations beyond I1–I5)

The run() tests were red before these; each is required for small/multi-round runs to
terminate (and to resume in-process):

- **I6 — feed scan restarts at id 0 on every refill.** The old monotonic
  `last_candidate` cursor silently skipped candidates re-elected across rounds
  (a round-2 candidate `C` with `C.id <= last_candidate` was never re-listed), wedging
  multi-round groups even with I1 in place. `list_pending_comparisons` already guarantees
  exactly-once via `dedup_inflight`, restoring the scan from the top every refill.
- **I7 — the loop now closes the final batch in-loop.** The old feed loop applied outcomes
  only at `DRAIN_CHUNK` (5_000) and `break`t on an empty feed slice, so a run's last
  `< 5_000` outcomes were applied by the drain tail, the FSM never ran again, and the
  leftover `count_files_in_phase(prev)` panic fired for the canonicals. The loop now
  drains with `override_drain = true` every iteration (ragged tails apply immediately —
  batching is preserved while outcomes are plentiful), and an empty refill sets a
  `feed_exhausted` flag; it breaks only when the feed is exhausted, the buffer is drained,
  and `dequeued_total == feed_total`. The drain tail remains as the thread-lifecycle
  owner (join + flush).
- **I8 — `create_temp_dedup_table` clears `dedup_inflight`.** Abandoned in-flight markers
  (pair fed, outcome never produced after an interrupt) otherwise wedge a same-process
  resume; in production a resume is a new process (TEMP gone) so this is latent there. The
  marker set is now emptied on (re-)create, matching the documented "blank marker set at
  phase start" invariant. (`run_force_interrupt_…`/`run_graceful_interrupt_…` resume via `run()`
  on the same `Database`.)

### Test-level implementation notes (deviations from the inventory)

- Interrupt injection happens at the **loop level** (bars owned by the test, trigger threads
  capture only by-value clones) because a `thread::spawn(move || …)` cannot borrow
  `world.progress` to lazily acquire the bar. The run()-level `Err(Interrupted)` tail is
  pinned by `run_interrupt_before_start_returns_interrupted`; the loop-level tests still
  assert table-survival + resume-to-completion via `run()`.
- The trigger signal is **`bar.length() > 0`**, not `position() > 0`: the dedup worker
  sets the bar length per pair but never increments `position`, and the bar starts at
  `Some(0)`. `Some(0)` is the initial config value — length must be *positive* to mean
  "worker processing a pair".
- `run_graceful_interrupt_…` does **not** assert `completed >= 1`: graceful can legitimately
  land in the window between the worker's top-of-loop `check_between_files` and
  `compare_one`'s own gate, yielding zero applied outcomes. Both groups use 16 MiB pairs so
  an unstarted group always remains at interrupt time (`count_pending >= 1` is stable).
- `run_fail_fast_on_errored_group` asserts `ErrorWhileDedup` on the **failed candidate** only
  (the canonical side stays clean).
- `finish_to_done`/`finish_to_error` return **SQLite matched-row counts**, which include
  already-`deduped` group members (`3` for a 3-member group where 2 were pre-linked).
- `run_enqueue_dequeue_loop_dedup` signature changed to
  `(rt, send, recv, mut handles: Vec<thread::JoinHandle<()>>)` (handle ownership), replacing
  the `one_running` closure + `&Vec<…>` pair.