# Plan: Rehash-phase tests (unarchive/rehash.rs + db/rehash.rs)

Status: **agreed, ON HOLD** (2026-10-02). Do not implement yet. Sequencing decision:
the user wants a **generic send/receive drain loop** (closures parameterized over the
feed/apply/worker-outcome types) built and tested **first**, so this loop's interrupt
semantics get tested once and rehash can reuse that abstraction instead of duplicating
the tests. This plan pins the *behavioral contracts* rehash must keep through that
refactor, plus the rehash-specific tests that remain regardless.

Scope: tests only, **plus exactly one** production-code change (join the workers inside
the drain loop before `drop(recv)`; see §Decisions). No rehash refactors beyond that.

## Why / what this guards

The extract `rehash` phase was rebuilt on the crossbeam producer/consumer pattern
(`03e7f5c`): worker threads SHA-1 each extracted cache payload and compare against the
catalog digest; outcomes are batch-ingested into the work DB; the phase is resumable.
There is **no test coverage yet** for the rehash pipeline or its DB layer. These tests pin:

1. `hash_file` I/O roundtrip: digest in, `FileStat` errors out, `Interrupted` (force) out.
2. `rehash_one` outcome mapping: `Match`/`Mismatch`/`Errored`, force-abort `None`,
   panic on missing `tar_member_name` (invariant), panic on unexpected error variants.
3. `rehash_worker` exit contract: interruption, in-file force abort, closed channel —
   all three break paths MUST emit the trailing `None` exit marker.
4. `handle_send_receive_loop`: batch commit, graceful (finish in-flight + persist, no
   new scheduling), force (discard in-flight, apply already-delivered outcomes), resume.
5. `run`: `skip_rehash`, `pending == 0` short-circuit, mismatch endgame, `fail_fast`
   endgame, interrupt tails (queue survives), success (queue dropped).
6. `db/rehash.rs` SQL: stable counts, `promote_unrehashable_files` null-safe negation,
   queue ordering/idempotency, pull phase predicate, outcome ingest flags+phase,
   `skip_rehash` scope.

## Decisions locked with the user (2026-10-02)

**D1 — Force/graceful final drain = copy the dedup/sparsify approach.**
Survey of the four loop copies:
- `archive/hash.rs` and `unarchive/rehash.rs`: phase-2 loop breaks on
  `is_interrupted() || dequeue==feed || !one_running() || exited==jobs`, then **one**
  capped `drain_chunk(..., true)`, `drop(recv)`, flush; **no worker join**.
- `archive/dedup.rs` / `archive/sparsify.rs`: identical break + single capped final
  drain, but the loop **owns `mut handles: Vec<JoinHandle<()>>`**, derives
  `at_least_one_running` from it, and **joins every worker** before the final drain and
  `drop(recv)` (sparsify: "joined before the final drain so a trailing `None` can never
  race `drop(recv)`").

The no-join variants race: on **graceful**, an in-flight worker may `out.send(Some(_))`
*after* `drop(recv)` → `expect("result channel closed")` panic in the worker thread and
the outcome is lost. Production change for rehash (matches dedup/sparsify):

- `handle_send_receive_loop(rt, send, recv, mut handles: Vec<thread::JoinHandle<()>>)`
  instead of the `one_running` closure; call
  `at_least_one_running(&handles.iter().collect())` internally.
- After the phase-2 loop: `for handle in take(&mut handles) { let _ = handle.join(); }`,
  then the existing single `drain_chunk(&mut busy, true, …)`, `drop(recv)`,
  `recorder.flush()`, return counts.
- `run` drops the `is_running` closure and passes `thread_handles` by value.

Keep the **single capped** final drain (same as dedup/sparsify). Known residual: on
**force** with > `DRAIN_CHUNK` already-delivered outcomes, the overflow is dropped
(rows stay pending → redone on resume; no correctness issue). Recorded as a follow-up
for the generic loop, where it can be reconciled uniformly across phases. `hash.rs` has
the same no-join race — also a follow-up (out of rehash scope).

**D2 — Generic loop comes first.** The user will build a closure-parameterized
`send_receive_loop` (feed / worker-outcome / apply as closures; core state machine
shared) and test it once. Rehash will adopt it. This plan's loop-level tests
(`handle_send_receive_loop` section) must be *rewritten against that abstraction* at
implementation time; the behavioral contracts listed here are the acceptance criteria.
The DB/`hash_file`/`rehash_one`/`rehash_worker`/`run` tests are unaffected.

**D3 — Dev profile only.** No `#[cfg(release)]` gating anywhere in the repo; assume
identical behavior in dev and release. Run everything under `cargo test -p tar-dedup
--lib` (dev profile, unwinds → `catch_unwind` works). NEVER `--release`
(`[profile.release] panic = "abort"` breaks `catch_unwind`).

## Current rehash shape (context to rebuild from)

`src/unarchive/rehash.rs`:
- `run(rt)` — `count_files_to_rehash` / `count_rehashed_files` (pending derived),
  `{do_skip}rehash pass` log; `!scan.rehash` → `skip_rehash` + `inc_global` + return;
  `set_phase_total/position`; `promote_unrehashable_files` + `inc_global`; `pending == 0`
  → return; `create_rehash_queue`/`populate_rehash_queue`;
  `create_thread_bars(BarKind::Bytes, effective_jobs())`; spawns `rehash_worker`s over
  `bounded::<StrippedRecord>(WORK_CAPACITY)` / `bounded::<Option<RehashOutcome>>(OUT_CAPACITY)`;
  `handle_send_receive_loop(...)`; `drop_thread_bars`; tail:
  - interrupted → `Err(Error::Interrupted)` ("force-aborted; in-flight progress
    discarded" / "stopped; completed files saved", `saved = matches+mismatches+errors`),
    queue KEPT;
  - success → `drop_rehash_queue`; `rehash complete` info; `mismatches > 0` →
    `Err(Config "Corruption detected: N files with mismatching hash. Ignore this error
    with --force")` unless `config.force`; `errors > 0` → `Err(Config "Encountered N
    errors while rehashing.")` when `process.fail_fast`, else warn.
- `handle_send_receive_loop(rt, send, recv, one_running) -> Result<RehashCounts>` —
  phase-1 feed loop, `drop(send)`, phase-2 drain loop, single final
  `drain_chunk(..., true)`, `drop(recv)`, `recorder.flush()`. `RehashCounts { matches,
  mismatches, errors }`; `apply_chunk` ingests via `ingest_rehash_outcome`, records
  `Errored(id, fse)` through `recorder.record_file(id, ERROR_PHASE, fse.recreate(),
  ErrorFlags::default())`, `inc_both(n)`. Constants `BATCH_SIZE=10_000`,
  `WORK_CAPACITY=BATCH_SIZE`, `OUT_CAPACITY=2*`, `FEED_CHUNK=1_024`,
  `DRAIN_CHUNK=BATCH_SIZE/2`.
- `rehash_worker(stage_dir, bar, shutdown, work, out)` — owns `buf = io_buffer()`;
  `Ok(row) => if shutdown.is_interrupted() { break }`, `bar.reset/set_length(row.size)`,
  `rehash_one(&mut buf, &stage_dir, &row, &shutdown, Some(&bar))`:
  - `Some(outcome)` → `out.send(Some(outcome))`;
  - `None` → `break` (force mid-file);
  `Err(_) => break`; after loop `out.send(None)` (exit marker).
- `rehash_one(buf, stage_dir, record, shutdown, pb) -> Option<RehashOutcome>` —
  `tar_member_name().expect("Archived files need to have a tar_member_name")`;
  `hash_file(...)`: `Ok(d) →` compare vs `record.sha1`
  `.expect("PRECONDITION FAILED: rehash requires sha1 to be present")` → `Match`/
  `Mismatch`; `Err(Error::Interrupted) → None`; `Err(e @ Error::FileStat(_)) →`
  `Some(Errored(record.id, e.to_only_file_stat()?))`; other `Err(e) → panic!`.
- `hash_file(path, buf, shutdown, pb) -> Result<[u8; 20]>` — `File::open` (Io),
  loop `check_in_flight()?` (Interrupted on force only), `read`, sha1 update, `pb.inc(n)`.

`src/db/rehash.rs`:
- `files_to_rehash_where()` (param `:extracted`): `canonical_id = id AND ftype='file'
  AND sha1 IS NOT NULL AND (flags & :extracted) != 0 AND {generate_archive_and_extract_filter(None)}`.
- `count_files_to_rehash` (phase-independent) / `count_rehashed_files` (AND phase='rehashed').
- `promote_unrehashable_files` — null-safe disjunct: `canonical_id IS NULL OR
  canonical_id != id OR ftype != 'file' OR sha1 IS NULL OR (flags & :extracted) = 0 OR
  NOT ({joint filter})`, target `phase='extract_filtered'` → `'rehashed'`.
- `create/populate/pull/drop_rehash_queue` (mirror of sparsify queue; pull has phase-only
  predicate `files.phase = 'extract_filtered'`).
- `RehashOutcome { Match(FileId), Mismatch(FileId), Errored(FileId, FileStatError) }`.
- `ingest_rehash_outcome(conn, &[RehashOutcome])` — one tx: Match → phase `rehashed`;
  Mismatch → `RehashMismatch` flag + phase; Errored → `ErrorWhileRehashing` flag + phase.
- `skip_rehash` — `UPDATE files SET phase='rehashed' WHERE phase='extract_filtered'`.

Schema facts the fixture needs (`db/schema.rs`): `filter_reason_archive`/
`filter_reason_extract` both get a seeded id-0 row at init; include rows must be `< 0`,
exclude `0`. `files.include_reason_*`/`exclude_reason_*` default 0; `flags` default 0.
`generate_archive_and_extract_filter` = include `< 0`, exclude `= 0` for both tables —
rows must be seeded to pass or `promote_unrehashable_files` sweeps them unread.

## Fixture (bottom of `unarchive/rehash.rs`)

`struct ExtractTestWorld` (mirrors archive/hash.rs `TestWorld`, extract-flavored):
- `tempfile::tempdir()` kept alive as `dir`.
- `db: Database::open(db_path)` where `db_path = config.paths.db_path()` (work dir).
- `config: ExtractConfig::for_scan_test(...)` (in `config/extract.rs`, already
  `#[cfg(test)]`; `jobs = io_jobs = 1` → `effective_jobs() == 1`, `scan.rehash = true`),
  overridden per test: `force`, `process.fail_fast`, `process.no_errors`, `scan.rehash`.
- `progress: ProgressBarSet::new(EXTRACT_MULTIPLIER)`; `shutdown: Shutdown::detached()`.
- `fn rt(&self) -> ExtractRTArgs`; a `Shutdown` clone for trigger threads is shared
  (mode is `Arc<AtomicU8>`), so `request_graceful()`/`request_force()` act on the same
  instance the loop/workers read.
- `add_elected(name, payload)`:
  1. `insert_file(&NewFileRecord { abs_path: <seed path>, ext: original_extension,
     size: payload.len(), ftype: Some(FileType::File), ..None })` → look up `FileId`.
  2. `mark_self_canonical(id)` (`canonical_id = id`).
  3. `set_file_flag(id, FileFlag::FileExtracted, true)`.
  4. `update_file_inspection_per_id(id, sha1_of(payload), 0, false)` (sets `sha1`).
  5. `mark_file_phase(id, FilePhase::ExtractFiltered)`.
  6. seed `INSERT OR IGNORE INTO filter_reason_archive(id,source,line,expression) VALUES
     (-1,'internal',NULL,'.*')` (+ same for `filter_reason_extract`); then set
     `include_reason_archive = -1, exclude_reason_archive = 0, include_reason_extract =
     -1, exclude_reason_extract = 0` for the row (raw SQL inside `db.with_transaction`).
  7. write the payload to `stage_dir / <record's tar_member_name()>` (compute from the
     inserted `FileRecord`/`StrippedRecord`).
- `add_dupe(name)` — electable canonical + a second row `canonical_id != id`
  (e.g. `set_canonical(dupe, canonical)`), same phase/flags → swept by promote.
- `add_non_file(name)` — `ftype = 'dir'` row → swept by promote.
- Keep `effective_jobs >= 1` in every test config (`create_thread_bars` `debug_assert!`s
  `count > 0`).

For the loop-test harness (mirrors hash `run_loop`): spawn one `rehash_worker`, pass
`vec![handle]` into `handle_send_receive_loop`, always `progress.create_thread_bars`
before spawning and `drop_thread_bars` after (matches `run`).

## Tests

### `src/db/rehash.rs` (bottom; raw `Connection` + `crate::db::schema::initialize`, sparsify.rs style)

Seed both `filter_reason_*` tables with the `-1` include row. Helper
`insert_elected(conn, id, size, sha1)` inserts
`files(id, abs_path, ext, size, ftype='file', phase='extract_filtered', sha1,
 include_reason_archive=-1, exclude_reason_archive=0, include_reason_extract=-1,
 exclude_reason_extract=0, flags=<FileExtracted mask>, canonical_id=id)`.

1. `counts_stable_across_phase` — `count_files_to_rehash == elected` regardless of
   phase; after marking one `rehashed`, `count_files_to_rehash` unchanged and
   `count_rehashed_files` +1.
2. `promote_covers_every_or_arm` — one row per failing arm (canonical NULL /
   canonical != id / ftype dir / sha1 NULL / flag unset / extract-filter fail) + one
   keeper; `promote_unrehashable_files` → all promoted to `rehashed`, keeper stays
   `extract_filtered`.
3. `queue_populate_orders_size_desc` — sizes 4 MiB / 4 MiB+1 / 1 MiB → positions
   `[(2,1),(1,2),(3,3)]`; `populate_rehash_queue` idempotent across two calls.
4. `pull_skips_rehashed_rows` — a `rehashed` row (and a row with `ErrorWhileRehashing` +
   `rehashed` phase) is excluded; slice-walk from the returned position reproduces the
   (now empty) tail.
5. `ingest_flags_and_phase` — batch `[Match, Mismatch, Errored]`: Match → `rehashed`, no
   flag; Mismatch → `RehashMismatch` + `rehashed`; Errored → `ErrorWhileRehashing` +
   `rehashed`.
6. `skip_rehash_promotes_extract_filtered_only` — `extract_filtered` rows advance,
   `unarchived`/already-`rehashed` rows untouched.

### `src/unarchive/rehash.rs` (bottom)

**`hash_file`**
1. `hash_file_returns_sha1` — temp payload; `hash_file(path, &mut io_buffer(),
   &Shutdown::detached(), None)` → `Ok(sha1_of(payload))`.
2. `hash_file_open_error_propagates` — nonexistent path → `Err(Error::FileStat(Io))`.
3. `hash_file_force_interrupts` — `shutdown.request_force()` before the call → `Err(
   Error::Interrupted)` (first `check_in_flight`).
4. `hash_file_graceful_continues` — `request_graceful()` → still `Ok` (graceful never
   aborts in-flight).
5. (optional) `hash_file_pb_advances_bytes` — pass a `thread_bar` clone; assert
   `position` advanced by full size.

**`rehash_one`**
6. `rehash_one_panics_on_missing_member` — record with `canonical_id != id` (or
   `ftype != File`); `catch_unwind` returns `Err(_)` (the `expect`).
7. `rehash_one_match_and_mismatch` — elected record, payload matching `sha1` →
   `Some(Match)`; corrupt payload → `Some(Mismatch)`.
8. `rehash_one_file_error_is_errored` — payload file missing → `Some(Errored(id,
   FileStatError::Io))` (via `to_only_file_stat()?`; recovers → `Ok` mapping).
9. `rehash_one_force_abort_returns_none` — `request_force()` + >4 MiB file → `None`
   (interrupt is swallowed, no `Errored` produced).
10. `rehash_one_panics_on_unexpected_error` — construct a `FileStat`-free `Error` path
    is not reachable from `hash_file`; assert the invariant arm by
    `catch_unwind` on a stubbed `Error::Config` if a seam exists, else drop (document).

**`rehash_worker`** (drive channels directly; `stage_dir` with one payload)
11. `rehash_worker_sends_outcome_then_none_on_channel_close` — send one row, receive
    `Some(RehashOutcome)`; `drop(work_send)` → receive `Ok(None)` exit marker.
12. `rehash_worker_interrupt_breaks_and_sends_none` — `request_interrupted` before
    handing a row → no outcome sent, one `Ok(None)` exit marker.
13. `rehash_worker_force_midfile_breaks_and_sends_none` — big file + `request_force()`
    mid-read (bar-position poll) → `rehash_one` yields `None` → break; no `Some`
    sent, exactly one `Ok(None)` exit marker.

**`handle_send_receive_loop`** (rewritten against the generic loop once D2 lands; these
contracts are the acceptance criteria)
14. `loop_processes_batch` — 3 elected rows (cap-2 work chan), one worker → `counts
    {matches:3}`, all rows `rehashed`, queue positions in size-DESC.
15. `loop_graceful_mid_feed_finishes_in_flight_and_resumes` — ~8 rows, 1 worker; trigger
    via thread bar (`position > 0 → 0`) then `request_graceful()`; loop returns `Ok`,
    `completed >= 1 && < 8`; in-flight outcome persisted; fresh detached shutdown +
    resumed loop finishes the rest; no double-hash (stored digest == payload digest);
    `rehash_queue` survives the interrupt (drop lives in `run`).
16. `loop_force_discards_in_flight_but_drains_delivered` — 1 worker + big in-flight file
    + a couple small already-delivered; `request_force()` mid-read; in-flight row stays
    `extract_filtered`, no flags, no error-log row; already-delivered outcomes applied;
    resumed run re-verifies the remaining row.

**`run`**
17. `run_skip_rehash_promotes_everything` — `scan.rehash = false`; elected + dupe +
    dir rows all → `rehashed`; `Ok`; `rehash_queue` never created (empty
    `pull_pending_rehash_rows` / absent from `sqlite_master`).
18. `run_pending_zero_returns_ok` — no electable rows (or all already `rehashed`) → `Ok`
    without touching payloads.
19. `run_completes_and_drops_queue` — elected rows with matching payloads → `Ok`, all
    `rehashed`, `rehash_queue` dropped.
20. `run_mismatch_endgame` — one corrupted payload → `Err(Config("Corruption detected:
    …"))`; with `config.force = true` → `Ok` + `RehashMismatch` flag set.
21. `run_errors_fail_fast` — one missing payload → error recorded in `errors` table
    (via `get_records_by_file_id`), `ErrorWhileRehashing` set; `fail_fast = false` →
    `Ok`; `fail_fast = true` → `Err(Config("Encountered 1 errors while rehashing."))`.
22. `run_graceful_interrupt_tail` / `run_force_tail` — interrupt mid-run → `Err(
    Error::Interrupted)`; completed rows persisted; `rehash_queue` survives; a follow-up
    `run` with detached shutdown completes the phase.

## Build / test commands

- `cargo build -p tar-dedup -p tar-dedup-cli` (green, no new warnings from touched files).
- `cargo test -p tar-dedup --lib` (dev profile; NEVER `--release` — `panic="abort"`
  breaks `catch_unwind`). Pre-existing failing tests (scan/inventory WIP) are out of
  scope; fix only if the touched files block the build.
- Targeted: `cargo test -p tar-dedup --lib -t 'unarchive::rehash::tests::…'`.

## Gotchas / notes for the implementer

- Interrupt triggers must be poll/converge based (bar `position > 0 → 0`, or `length() >
  0` set before `rehash_one`), never bare sleep-then-hope. Keep files ≤ 32 MiB.
- The `0o000`-style permission tests are not needed here (missing file already yields an
  `Errored` Io outcome deterministically).
- `rehash_one` needs a valid `sha1` on the record — the `expect` fires otherwise; always
  set digests via `update_file_inspection_per_id` in fixtures.
- `ingest_rehash_outcome` runs in one tx; a panic inside it leaves nothing committed —
  fine for `catch_unwind` assertions.
- The join-in-loop change makes worker exits deterministic before `drop(recv)`; without
  it tests 15/16/22 are flaky (worker `out.send` can panic on a dropped receiver).
- `errored` vs `Errored` naming: the DB enum is `Errored(FileId, FileStatError)`; the
  Recorder path calls `fse.recreate()` to re-own the error log row (kind + message).

## Follow-ups / as-built

- **Generic drain loop (D2)** — the precondition plan: closure-parameterized
  send/receive loop (feed / outcome / apply closures), tested once, adopted by
  hash/dedup/sparsify/rehash. Reconcile the force `> DRAIN_CHUNK` overflow and the
  hash.rs no-join race there.
- This plan is picked up again after the generic loop lands; §handle_send_receive_loop
  tests are rewritten against it, the rest stand.
- (as-built section to be filled at implementation time: bugs flushed out + fixes.)