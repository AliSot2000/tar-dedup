# Plan: Rehash-phase tests (unarchive/rehash.rs + db/rehash.rs)

Status: **implemented + green** (2026-10-03). All 28 tests land below; only outstanding
change is the flagged `db/schema.rs` CHECK fix (see As built). Build:
`cargo test -p tar-dedup --lib` → **272 passed / 4 failed** (the 4 = pre-existing
`db::inventory` + `unarchive::scan` WIP-test failures, out of scope). The generic
send/receive drain loop (`plans/unified-send-receive-loop.md`, Stage 1) is built, tested
(G1–G10), and adopted by hash/dedup/sparsify/rehash. Rehash's `handle_send_receive_loop`
is now a thin closure wrapper over `common::send_receive_loop`. This suite is therefore
written **against the real wiring** (rehash's wrapper + its closures), not a future
abstraction. The loop-mechanics tests that overlap the generic suite (G1–G10) were
already dropped at the Stage-1 boundary; everything here pins *behavioral contracts* and
rehash-specific SQL.

Scope: tests **plus** the one schema CHECK fix below (required by the test data itself and
a latent extract-filter bug — see As built).

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

## Decisions locked with the user (2026-10-02, updated 2026-10-03)

**D1 — Force/graceful final drain = copy the dedup/sparsify approach — SUPERSEDED, DONE.**
`plans/unified-send-receive-loop.md` delivered the shared loop, which **joins every worker
then drains the results queue to empty before `drop(recv)`** (phase 2/3). That subsumes the
old join-before-drop fix for rehash **and** fixes the two "known residuals" the old D1
recorded: the force `> DRAIN_CHUNK` overflow is now fully drained (nothing dropped) and
hash's no-join race is gone. Rehash's `handle_send_receive_loop(rt, send, recv,
thread_handles: Vec<JoinHandle<()>>)` owns the join transitively (it passes the handles into
`send_receive_loop`); `run` dropped its `is_running` closure. Nothing further to do here.

**D2 — Generic loop comes first — LANDED.** `common::send_receive_loop` exists and rehash
adopts it. §handle_send_receive_loop tests below therefore drive rehash's **wrapper**
(a test-only `run_loop` harness, mirroring **sparsify's kept harness** — `hash.rs`'s
original harness was deleted at Stage 1). The `u64` the shared loop returns (committed
outcomes) equals `matches + mismatches + errors` for rehash (every `Some` outcome is
applied and counted), but that cross-check is an internal invariant — the wrapper consumes
it, so it is not directly assertable; skip it (decision: don't add production plumbing).

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

`struct ExtractTestWorld` (mirrors the kept sparsify/dedup fixtures, extract-flavored):
- `tempfile::tempdir()` kept alive as `dir`.
- `work_dir = dir.path().join("estage")`, **`std::fs::create_dir_all` before
  `Database::open`** (extract `db_path = work_dir/tar-dedup.sqlite`; unlike hash's
  TestWorld there is no pre-made temp file).
- `db: Database::open(&config.paths.db_path())`.
- `config: ExtractConfig::for_scan_test(...)` (`#[cfg(test)]`) — **note it defaults
  `force = true`**; tests override `config.force = false` where the strict endgames are
  asserted. `jobs = io_jobs = 1` → `effective_jobs() == 1`, `scan.rehash = true`,
  `process.fail_fast = false`, `no_errors = false`.
- `progress: ProgressBarSet::new(EXTRACT_MULTIPLIER)`; `shutdown: Shutdown::detached()`.
- `fn rt(&self) -> ExtractRTArgs`; `fn rt_with<'a>(&'a self, &'a Shutdown)` for resumes
  (a `Shutdown` clone for trigger threads is shared — `Arc<AtomicU8>` — so
  `request_graceful()`/`request_force()` act on the same instance the loop/workers read).
- `seed_filter()` — `INSERT OR IGNORE` the `-1` include row into **both**
  `filter_reason_archive` **and** `filter_reason_extract` (the joint filter
  `generate_archive_and_extract_filter` references all four columns).
- `add_elected(name, payload)`:
  1. `insert_file(&NewFileRecord { abs_path: <seed path>, ext: original_extension,
     size: payload.len(), ftype: Some(FileType::File), ..None })` → look up `FileId`.
  2. `mark_self_canonical(id)` — **note: this also sets `phase='deduped'`**; the phase is
     re-set below, pin that order.
  3. `set_file_flag(id, FileFlag::FileExtracted, true)`.
  4. `update_file_inspection_per_id(id, sha1_of(payload), 0, false)` — sets digest **and
     `phase='hashed'`**; again re-set below.
  5. `mark_file_phase(id, FilePhase::ExtractFiltered)` (last phase write).
  6. set the row's reason columns via raw SQL: `include_reason_archive = -1,
     exclude_reason_archive = 0, include_reason_extract = -1, exclude_reason_extract = 0`.
  7. write payload to `work_dir / <record.tar_member_name()>` — **`stage_dir() ==
     work_dir`** (flat `.estage`, `config/paths.rs`).
- `add_dup(name, canonical_id)` / `add_non_file(name)` — raw-insert rows the promote sweep
  covers (canonical `!= id` / `ftype='dir'`), same phase; no stage payload needed.
- `prepare_for_rehash()` — `run()` preamble minus bars/workers: `promote_unrehashable_files`,
  `create_rehash_queue`, `populate_rehash_queue`. Loop tests call the harness, not `run`.
- Keep `effective_jobs >= 1` (`create_thread_bars` `debug_assert!`s `count > 0`).

For the loop-test harness (mirrors **sparsify's kept `run_loop`**): spawn one
`rehash_worker`, `handles.push(worker)`, pass `vec![handle]` into
`handle_send_receive_loop`, and always `progress.create_thread_bars` before spawning +
`drop_thread_bars` after. Returns `RehashCounts`.

## Tests

### `src/db/rehash.rs` (bottom; raw `Connection` + `crate::db::schema::initialize`, sparsify.rs style)

`open_db()` seeds the `-1` include row into **both** `filter_reason_archive` **and**
`filter_reason_extract` (sparsify only seeded archive). Helper `insert_elected(conn, id,
size, sha1)` inserts
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
3. `queue_populate_orders_size_desc` — sizes 4 MiB / 4 MiB+1 / 1 MiB → queue order
   `[(2,1),(1,2),(3,3)]` as **(id, position)** (file 2 → position 1, etc.; sparsify
   asserts the same shape); `populate_rehash_queue` idempotent across two calls.
4. `pull_skips_rehashed_rows` — a `rehashed` row (even carrying `ErrorWhileRehashing`)
   is excluded by the phase predicate; slice-walk from the returned position reproduces
   the (now empty) tail.
5. `ingest_flags_and_phase` — batch `[Match, Mismatch, Errored]` (`Errored` carries a
   fabricated `FileStatError`): Match → `rehashed`, no flag; Mismatch →
   `RehashMismatch` + `rehashed`; Errored → `ErrorWhileRehashing` + `rehashed`.
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

**`handle_send_receive_loop`** (driven through the rehash wrapper by the `run_loop`
harness; the shared loop's generic mechanics are covered by G1–G10 — these pin the rehash
wiring)
14. `loop_processes_batch` — 3 elected rows (cap-2 work chan), one worker → `counts
    {matches:3}`, all rows `rehashed`, queue positions in size-DESC.
15. `loop_graceful_mid_feed_finishes_in_flight_and_resumes` — ~8 rows, 1 worker; trigger
    via thread bar (`position > 0 → 0`) then `request_graceful()`; loop returns `Ok`,
    `completed >= 1 && < 8`; in-flight outcome persisted; fresh detached shutdown +
    resumed loop finishes the rest; no double-hash (stored digest == payload digest);
    `rehash_queue` survives the interrupt (drop lives in `run`).
16. `loop_force_discards_in_flight_but_drains_delivered` — sizes chosen so a **bigger**
    file is already delivered before the in-flight file: 1 worker + a delivered 64 MiB +
    an in-flight 32 MiB (size-DESC puts 64 MiB first) + two smalls still queued;
    `request_force()` when the bar's `length()` moves onto the 32 MiB file; in-flight row
    stays `extract_filtered`, no flags, no error-log row; the delivered row IS applied
    (exact applied-set assert — guaranteed by the shared loop's drain-to-empty, formerly
    the force `> DRAIN_CHUNK` residual); resumed run verifies the rest.

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

- Interrupt triggers must be poll/converge based (bar `position > 0 → 0`, or `length()`
  moving onto the next file), never bare sleep-then-hope. Keep files ≤ 32 MiB (64 MiB only
  for test 16's delivered row).
- The `0o000`-style permission tests are not needed here (missing file already yields an
  `Errored` Io outcome deterministically).
- `rehash_one` needs a valid `sha1` on the record — the `expect` fires otherwise; always
  set digests via `update_file_inspection_per_id` in fixtures. That helper **also sets
  `phase='hashed'`**, and `mark_self_canonical` **sets `phase='deduped'`** — the fixture
  must `mark_file_phase(ExtractFiltered)` *after* both (order pinned in §Fixture).
- `ingest_rehash_outcome` runs in one tx and requires `&mut Connection`.
- `for_scan_test` defaults `force = true`; set `config.force = false` only in
  `run_mismatch_endgame`'s strict branch (and the tail tests if they assert the strict
  error — they don't).
- The join + drain-to-empty behind `send_receive_loop` makes worker exits deterministic
  before `drop(recv)`; tests 15/16/22 no longer need the old flaky-workaround framing.

## Follow-ups / as-built

- **Stage 3** (outside scope): extract `place` ("materialize") loops onto
  `send_receive_loop` — the last adopter.
- This Stage-2 suite is the final test battery for rehash; no remaining production
  change for rehash.

### As built (2026-10-03)

Implemented and green. All 28 tests landed (6 × `db::rehash` + 22 × `unarchive::rehash`);
full lib suite is **272 passed / 4 failed**, the 4 being the pre-existing
`db::inventory` + `unarchive::scan` WIP failures (untouched).

Bugs flushed out / deviations:

1. **Production change — schema CHECK typo (out of the "tests only" scope, but required).**
   `db/schema.rs` constrained `include_reason_extract >= 0`, contradicting the
   `include_reason_extract < 0` include-rule semantics used by both
   `generate_archive_and_extract_filter` and `db/filter.rs:107` (which writes `-1`) —
   and the same-level `include_reason_archive` is `<= 0`. Fixed the extract column to
   `CHECK (include_reason_extract <= 0)`. Without it the test data (and, latently, the
   extract filter pipeline) cannot store an include rule.
2. **`add_dup`/`add_non_file` fixtures** are raw-INSERT (not `set_canonical`, which would
   force `phase='deduped'`); the FK to `files(id)` means a dup must reference a real
   canonical id.
3. **Mid-`run()` interrupt triggers are not feasible** (run() materializes its worker
   bars internally; `ExtractTestWorld` is not `Sync`, so no cross-thread DB/bar polling).
   The repo convention is used: `run_interrupt_tail` = pre-set interrupt → `run` returns
   `Err(Interrupted)` + queue survives + detached-shutdown `run` completes the phase. The
   mid-run interrupt *behavior* is pinned by the loop tests (`loop_graceful...`,
   `loop_force...`).
4. **Graceful trigger via `bar.length() > 0`**, not the `position > 0 → 0` reset-window
   (the reset window is too brief to poll reliably).
5. **`rehash_one_panics_on_missing_member`** seeds a canonical first so the dup's own row id
   differs from `canonical_id` (previously the dup became accidentally self-canonical — id 1
   == canonical 1 — and resolved a member name; no panic).
6. **`hash_file_pb_advances_bytes`** asserts position before `drop_thread_bars`
   (`finish_and_clear` destroys the reading).
7. The loop-return `u64` ⇔ `matches+mismatches+errors` cross-check (D2 note) was **not**
   asserted: the rehash wrapper consumes the loop return, and adding plumbing for it would
   be production noise. (Noted in D2.)
8. `for_scan_test` defaults `force = true` — `run_mismatch_endgame` toggles it to `false`
   for the strict branch only.