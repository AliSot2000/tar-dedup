# Plan: Hash-phase unit tests (archive/hash.rs + db/hash.rs)

Status: **settled with the user** (2026-09-23). Scope is tests only, plus exactly one
production-code touch: `Shutdown::request_force()`. No refactors, no bugfixes to the
just-refactored hash pipeline. Tests live **inline** in `#[cfg(test)] mod tests` at the
bottom of the two source files; nothing goes to `tests/` (the tests need private access:
`hash_one`, `is_all_zero`, `record_hash_error`, `handle_send_receive_loop`, module constants).

## Why / what this guards

The hash phase was just refactored (worker state machine → `handle_send_receive_loop` in
`archive/hash.rs`, outcome ingestion → `ingest_hash_outcome` in `db/hash.rs`, with
`HashError{modified,id,err}` / `HashSuccess{modified,id,zero_pages,hash}` and
`HashingOutcome = Result<HashSuccess, HashError>`). These tests pin:

1. hashes are computed and stored in the DB, filesystem errors are stored (flag + error log),
2. interrupt handling in all three loop phases (feed+dequeue / dequeue-only / final drain),
3. triple-interrupt (force) aborts correctly,
4. modified-while-hashing detection (backend `FileFlag::Modified`),
5. size-DESC ordering (biggest files hashed first),
6. error classification in both `ingest_hash_outcome` and the recorder path: only
   `FileStat`/`Interrupted` tolerated; anything else (`Config`/`Other`/`Database`) panics,
7. the `sparse_count` / zero-page counter on files with large zero regions,
8. `--batch_size` stubs (placeholder tests to populate once the knob lands).

## Decisions locked with the user

- **File sizes ≤ 32 MiB.** The read buffer is 4 MiB (`IO_BUF_SIZE`), so max ~8 reads/file.
  No test may rely on "a 1 GiB file takes ≥1 s". Interrupt injection is **poll/converge
  based** (a helper thread polls an observable condition, then flips the shutdown), never a
  bare sleep-then-hope.
- **Panic assertions via `std::panic::catch_unwind`.** Verified: dev/test profile uses the
  default **unwind** panic strategy; `[profile.release] panic = "abort"` (`Cargo.toml:50`).
  Therefore: run tests with `cargo test` (default profile), NEVER `--release`. `err-to-panic`
  is an off-by-default optional feature (`crates/tar-dedup/Cargo.toml:9`) — irrelevant here.
  If `catch_unwind` is not exported under this dialect's path at implementation time, fall
  back to spawning a subprocess that triggers the panic and asserts a nonzero exit.
- **One production-code addition: `pub fn Shutdown::request_force(&self)`** (one line,
  mirrors `request_graceful`): `self.mode.store(MODE_FORCE, Ordering::SeqCst)`. Required
  because `MODE_FORCE` is otherwise only reachable through 3 real SIGINTs (which must not hit
  the test process). Put it in `src/shutdown.rs` next to `request_graceful`; add a tiny
  `#[cfg(test)] mod tests` there too (construct, `request_graceful`, `is_interrupted`;
  `request_force`, `is_force`; assert `check_in_flight` errors only after force).

## Current pipeline shape (context to rebuild from)

`src/archive/hash.rs`:
- `run(rt)` — counts, `promote_unhasheable_files`, `create_hash_queue`/`populate_hash_queue`,
  `progress.create_thread_bars(BarKind::Bytes, jobs)`, spawns `jobs` `hash_worker`s over
  `bounded::<StrippedRecord>(WORK_CAPACITY)` / `bounded::<HashingOutcome>(OUT_CAPACITY)`,
  then `let completed = handle_send_receive_loop(&rt, work_s, out_r)?`, drops bars, the
  double-canonical-dev-inode panic check, and the `is_interrupted()` tail (force vs graceful
  message; drops `hash_queue` only on success).
- `pub fn handle_send_receive_loop(rt, send: Sender<StrippedRecord>, recv: Receiver<HashingOutcome>) -> Result<u64>`:
  - creates its own `Recorder::new(rt.db, !rt.config.process.no_errors)`.
  - **Phase 1** loop: `if rt.shutdown.is_interrupted() { break }`; pull `FEED_CHUNK`
    (`pull_pending_hash_rows(queue_index, FEED_CHUNK)`); `send.try_send` each (advances
    `feed_total`); `drain_chunk(&mut busy, false)`; break when
    `feed_exhausted && feed_idx == feed_buf.len()`. `busy==false` → sleep 10ms.
  - `drop(send)` (workers end on disconnected `recv`).
  - **Phase 2** loop: interrupt check at top; `drain_chunk(&mut busy, false)`; break when
    `queue_index == feed_total`; sleep 10ms when idle.
  - **Phase 3** (final): `drain_chunk(&mut busy, true)` (drain_override applies even a
    partial batch); `drop(recv)`; `recorder.flush()`; return `completed`.
  - `drain_chunk|is_busy, drain_override|`: nonblocking `recv.try_recv` into `pending_out`
    (≤ `DRAIN_CHUNK` per batch), apply when `>= DRAIN_CHUNK || drain_override`.
  - `apply_chunk` closure: `rt.db.ingest_hash_outcome(&items, !rt.config.indexing.no_hardlink_detection)?`;
    per outcome `Ok(_) => ()`, `Err(e) => match &e.err { Interrupted => (), FileStat(_) =>
    record_hash_error(&mut recorder, &e), other => panic!(...) }`; `progress.inc_both(n)`.
- `record_hash_error(recorder, &HashError)` — `recorder.record_file(e.id,
  ErrorPhase::Pipeline(PipelinePhase::Hash), e.err.to_file_stat(None), ErrorFlags::default())`.
- `hash_worker(bar, page_size, shutdown, work, out)` — per row: `check_between_files`
  (break on Err), `bar.reset/set_length(row.size)/set_message`, 
  `let modified = warn_if_times_changed(...)`, `hash_one(...)` → `HashSuccess`/`HashError`
  (both carry `modified`), `out.send(res)`. Note: even `Error::Interrupted` from `hash_one`
  is sent as an `Err(HashError{err: Interrupted})` outcome — discarded later by ingest.
- `hash_one(buf, path, page_size, shutdown, pb)` — sha1 + zero-page count; full pages of
  zeros only; `carry_len`/`carry_zero` across reads; `check_in_flight()` per read.
  `Route::foo` etc. not relevant. Returns `Result<([u8; 20], u64)>`.
- constants: `BATCH_SIZE=10_000`, `WORK_CAPACITY=BATCH_SIZE`, `OUT_CAPACITY=2*`,
  `FEED_CHUNK=1024`, `DRAIN_CHUNK=BATCH_SIZE/2`.

`src/db/hash.rs`:
- types `HashError`/`HashSuccess`/`HashingOutcome` (top of file).
- `ingest_hash_outcome(conn: &mut Connection, results: &Vec<HashingOutcome>, update_hardlinks: bool) -> Result<u64>`:
  in ONE transaction, per outcome:
  - `Ok(hs)` → if `hs.modified` set `FileFlag::Modified`; always
    `update_file_inspection_per_id(&tx, hs.id, hs.hash, hs.zero_pages, update_hardlinks)`.
  - `Err(he)` → if `he.modified` set Modified; match `&he.err`: `Interrupted => ()`,
    `FileStat(_) => set ErrorWhileHash`, `other => panic!("Invariant Error...")`.
- `update_file_inspection_per_id(conn, file_id, digest, sparse_count, update_hardlinks)`:
  sets `sha1`, `sparse_count`, `phase='hashed'`; with `update_hardlinks=true` rowset =
  `WHERE (dev, inode) IN (SELECT dev, inode FROM files WHERE id = :id)`.
- `pull_pending_hash_rows(conn, index, limit)` — queue order (size DESC), filters
  `sha1 IS NULL` and no `ErrorWhileHash`.
- `hash_queue` table: `(id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL UNIQUE REFERENCES files(id))`,
  populated via `row_number() OVER (ORDER BY size DESC, id)`.
- DB schema facts required by the fixture (from `db/schema.rs`): `files(include_reason_archive
  INTEGER DEFAULT 0 CHECK (<= 0), exclude_reason_archive DEFAULT 0, phase DEFAULT
  'inventoried', ftype TEXT, flags INTEGER DEFAULT 0, sha1 BLOB, sparse_count INTEGER)`.
  `generate_archive_filter` = `include_reason_archive < 0 AND exclude_reason_archive = 0`,
  so **rows must have a negative `include_reason_archive`** or `promote_unhasheable_files`
  sweeps them to `hashed` unread — use `db.apply_no_filter()` after inserts.

## Fixture (bottom of `archive/hash.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    // crate:: imports: config::{ArchiveConfig, ...}, db::Database, progress, shutdown, types, error.
```
Helper `struct TestWorld`:
- `tempfile::tempdir()` (`dir` field kept alive).
- real files written with `std::fs::write(&path, &payload)`.
- `db: Database::open(dir.base.sqlite)`.
- per file: `db.insert_file(&NewFileRecord { ext: original_extension(&path), abs_path: path,
  size: <real bytes len>, ftype: Some(FileType::File), mtime/atime/ctime: None, ...all others
  None })`; then `db.apply_no_filter()` (once, after all inserts) so hashable_files_where
  (non-eager) matches `phase='inventoried' AND ftype='file'`.
- `config: ArchiveConfig` hand literal (struct + all nested structs are `pub`; mirror the
  `DEFAULT_CONFIG` literal in `config/archive.rs:390`), overriding:
  - `filter.eager_filter: false` → hash phase targets `inventoried`,
  - `indexing.no_hardlink_detection: true` → no `FileHardlinkCanonical` flag requirement
    (inserted rows have flags=0; real runs mark canonicals during inventory),
  - `process.io_jobs` and `process.jobs`: `Some(4)`,
  - `process.no_errors: false`,
  - `sparse.page_size: 4096`,
  - `paths.*`: temp paths (not exercised), `pipelines` defaults.
- `progress: ProgressBarSet::new(7)` (`ARCHIVE_MULTIPLIER`).
- `shutdown: Shutdown::detached()`.
- `fn rt(self) -> ArchiveRTArgs { ArchiveRTArgs { config: &self.config, db: &self.db,
  shutdown: &self.shutdown, progress: &self.progress } }`. The struct borrows — arrange so the
  world outlives each `run`/loop call (all owned by the test fn).
- recorded times are `None` → `warn_if_times_changed` returns early (no spurious `Modified`).

For interrupt tests the world also exposes the shutdown handle so a **trigger thread** can
flip it: `Shutdown` is `Clone` and the mode `Arc<AtomicU8>` is shared, so cloning into the
thread and calling `request_graceful()`/`request_force()` acts on the same instance the loop
and workers read.

## Tests — `src/db/hash.rs` (bottom)

Use `rusqlite::Connection::open(tmp)` + `crate::db::schema::initialize(&conn)`; insert rows
with raw `conn.execute("INSERT INTO files (id, abs_path, ext, size, ftype, phase, flags, dev, inode, include_reason_archive, exclude_reason_archive) VALUES (...)", [] )`
(id in 1..; `include_reason_archive = -1`, `exclude_reason_archive = 0`).

1. `ingest_writes_digest_phase_sparse_count` — Ok outcome (id, digest `[u8;20]`, zero_pages=7,
   modified=false); assert row `sha1 == digest`, `sparse_count == 7`, `phase == 'hashed'`,
   `(flags & 32) == 0` (no Modified).
2. `ingest_sets_modified_flag` — Ok outcome with `modified=true` → bit 5 set; and separately
   an Err outcome (`FileStat`) with `modified=true` → bit 5 set (proves the flag is set on
   both paths).
3. `ingest_filestat_error_sets_error_while_hash` — `Err(HashError{err: Error::FileStat(...),
   modified: false})` → bit 6 (`ErrorWhileHash`) set, `sha1 IS NULL`, `sparse_count == NULL`,
   `phase` untouched.
4. `ingest_discards_interrupted` — `Err(HashError{err: Error::Interrupted})` → row completely
   untouched (no sha1, no flags bits 5/6, phase unchanged).
5. `ingest_panics_on_invalid_error_variants` — for `Error::Config("x")`, `Error::Other(...)`,
   `Error::Database(...)`: `catch_unwind(|| { ingest_hash_outcome(&mut conn, &[outcome], false)?; })`
   must return `Err(_)` (a panic was caught). If `catch_unwind` is unavailable in the dialect:
   subprocess fallback (see Decisions). Constructor needs a `Connection` — note it panics
   inside the tx (`conn.transaction()` before the loop), so the panic is caught *after* the
   transaction begins — fine, nothing was committed.
6. (**B6**) `update_file_inspection_propagates_hardlink_group` — two rows with identical
   `dev`/`inode` values, distinct ids; call `update_file_inspection_per_id(conn, id_a,
   digest, 3, true)`; assert BOTH rows now have `sha1 == digest`, `sparse_count == 3`,
   `phase == 'hashed'`. (With `update_hardlinks=false`, only the exact id row updates.)
7. `batch_size_parity_stub` — `#[ignore]` + `TODO(batch_size)`: once batch size is plumbed,
   re-run tests 1/3/5 across a size matrix; assert identical DB outcome. (Verify the
   `#[ignore]` attribute name against the dialect at implementation time; if absent, use a
   doc-comment `// TODO(batch_size):`, or compile-gated dead code.)

## Tests — `src/archive/hash.rs` (bottom)

1. `run_hashes_and_stores_digests` — full `run(world.rt())`; files: random ~1 MiB, duplicate
   content ×2, empty file, 7-byte file. Assert Ok; every row `phase == 'hashed'`, `sha1` equals
   `Sha1::new()` over the file bytes computed in the test, `sparse_count == 0` for them.
2. `unreadable_file_recorded_not_fatal` — one file `0o000` (skip test early if euid == 0);
   `run` returns Ok; that row `sha1 IS NULL`, bit 6 set; `db.get_records_by_file_id(id)` has one
   record; the other files are hashed. Error must NOT abort the phase.
3. `modified_file_flagged` — one row inserted with recorded `mtime` deliberately stale (e.g.
   now − 1 h as RFC3339 via chrono formatting — check an existing writer, e.g.
   `db/inventory.rs` or `common/perms.rs`, for the RFC3339 format used by the schema) while the
   file on disk is current; run; assert that row has bit 5 (`FileFlag::Modified`). (We assert
   the backend flag, not the tracing warning — no log-capture seam.)
4. `hash_order_is_size_desc` — 3 files, sizes e.g. 8 MiB / 4 MiB + 1 B / 1 MiB. Assert:
   (a) `db.pull_pending_hash_rows::<StrippedRecord>(0, 100)` flattened ids == ids sorted by
   `size DESC` (re-sort in Rust within the test: map size→id, sort by size desc, compare);
   (b) `db.get_entries_to_hash::<StrippedRecord>(false, true, 100)` order matches the same;
   (c) `hash_queue` positions (`SELECT ... JOIN hash_queue ORDER BY hash_queue.id`) follow
   `size DESC, id`. This is the "huge files are started first" property (queue order = feed
   order = worker order).
5. `zero_page_counter_*` — call `hash_one(&mut buf, path, 4096, &Shutdown::detached(), None)`
   directly (module-private access):
   - `zero_page_counter_partial_last_page`: file = 3 full zero pages + (page−n) zero bytes →
     sparse_count == 3 (partial trailing window never counts).
   - `zero_page_counter_exact_multiple`: file = 40 pages of zeros → 40.
   - `zero_page_counter_mixed_content`: zeros then random → count == leading full zero pages.
   - `zero_page_counter_crosses_buffer_boundary`: file = `IO_BUF_SIZE + 2*page + (page-7)`
     all zeros → count == `(IO_BUF_SIZE + 2*page)/page` (constant `IO_BUF_SIZE` from
     `crate::common`); exercises the carry logic across reads.
   - integrated: one entry in the `run_hashes_and_stores_digests` tree with a known zero
     region (e.g. 4 pages zeros + 1 KiB random) → assert stored `sparse_count` == 4.
6. `interrupt_mid_feed` (**phase 1: enqueue + dequeue active**) — drive
   `handle_send_receive_loop` directly: 1 worker, ~8 files (a couple MiB each), channels
   `bounded::<StrippedRecord>(2)` (small work cap keeps feed backed up). Spawn a trigger
   thread that polls `files` (via a second `Connection` or the shared `Database`: read `sha1`
   until ≥ 1 row has `sha1 IS NOT NULL`), then `shutdown.request_graceful()`. Assert the loop
   returns Ok (not hang), `completed >= 1` and the count of hashed rows `< n` (some fed rows
   were dropped by the loop-top check); then re-run the loop against the same db with a **fresh
   detached shutdown** over remaining rows and assert all rows end hashed with matching digests
   (resume correctness, no double-hash). `hash_queue` must NOT be dropped by the interrupted
   loop (that drop lives in `run`'s success path).
7. `interrupt_dequeue_only` (**phase 2**) — 1 worker + 1 larger file (e.g. 32 MiB; feed is
   exhausted almost immediately, worker still hashing). Trigger thread: brief poll-equivalent —
   wait for `work` channel to drain (or just `request_graceful()` ~50–200 ms in; with ≤32 MiB
   the loop is in phase 2 or phase 3 by then, and the assertion is identical). Assert loop
   returns, `completed == 1`, file has a sha1 matching content (the "finish the in-flight
   file" guarantee — graceful never loses the in-flight outcome).
8. `interrupt_during_final_drain_applies_all` (**phase 3 no-loss**) — like #7 but the trigger
   fires later still (e.g. after an outcome is already sitting in `recv`). Final drain
   (`drain_override=true`) has no interrupt check, so every fed outcome is applied. Assert
   `completed == <fed>` and hashed rows == fed rows.
9. `triple_interrupt_force_aborts` — needs `Shutdown::request_force` (D1). 1 worker + 32 MiB
   file; trigger thread polls the worker's **thread bar** (`world.progress.thread_bar(0)` clone;
   poll `position() > 0`, which `hash_one` advances per 4 MiB read via `pb.inc(n)`), then
   `request_force()`. `hash_one`'s `check_in_flight()` aborts the read → worker sends
   `Err(HashError{err: Interrupted})` → discarded by ingest. Assert: loop returns without
   hanging, `completed == 0`, file row has `sha1 IS NULL`, NO bit 6, no error record. Note:
   if indicatif bar clones don't share `position()` state, poll alternatively for the file to
   be mid-hash via `length() > 0` (set before `hash_one`) — the abort then lands on
   `check_in_flight` or `check_between_files`, both yielding the same final state; verify at
   implementation and adjust the trigger signal. This test documents that force == "discard
   in-flight progress" (vs graceful == "finish in-flight").
10. `record_hash_error_persists_filestat` — construct a `Recorder::new(world.db, true)`; call
    the module-private `record_hash_error(&mut recorder, &HashError{ id, err: Error::FileStat(
    crate::error::FileStatError::Io { path, source: io::Error::new(EACCES...)}), modified:false})`,
    `recorder.flush()`, assert exactly one `errors` row for that file id (via
    `db.get_records_by_file_id`). Tolerated classifications: FileStat records, Interrupted
    discards — the panic on other variants is covered by db test 5.
11. `run_interrupts_tail_returns_interrupted` — (optional, ties the loop tests to `run`) full
    `run(rt)` with a trigger thread calling `request_graceful()` mid-run → returns
    `Err(Error::Interrupted)`, `hash_queue` still present (drop only on success). With
    `request_force` earlier → message path `is_force()`. Keep small (a few files); reuse world.
12. `batch_size_parity_stub` — `#[ignore]` + `TODO(batch_size)`: re-run #1/#4/#6 matrix once
    the knob exists (see db-side stub).

## Build / test commands

- `timeout 900 nix develop --command cargo build -p tar-dedup -p tar-dedup-cli` (green, no new
  warnings from the touched files).
- `timeout 900 nix develop --command cargo test -p tar-dedup --lib` (dev profile; NEVER
  `--release` — `panic="abort"` breaks `catch_unwind`).
- Targeted: `cargo test -p tar-dedup --lib -t <FQN>` as needed; e.g.
  `-t 'archive::hash::tests::interrupt_mid_feed'`.

## Gotchas / notes for the builder

- `std::fs::write(&path, &payload)` creates+overwrites the file (used in `common/files.rs`
  tests); use `Vec<u8>` payloads (random via a small LCG / `0..` pattern + zeros) up to 32 MiB.
- `insert_file` returns the `FileId` via lookup (see `tests/common/mod.rs::insert_file`), or
  query `file_id_by_abs_path`. Simpler: after inserts, `db.files_in_phase::<StrippedRecord>(FilePhase::Inventoried)`.
- chmod: use the `fs4`/`std::fs` permission API present in the codebase (e.g.
  `fs::set_mode(path, 0o000)` if the dialect exposes it; otherwise `FileExt`/`SetPermissions`).
- The `0o000` test must skip when euid==0 (`std::os::unix::geteuid()` if available; else skip
  entirely — CI may run as root).
- `handle_send_receive_loop` flushes the recorder itself; `run` calls
  `progress.drop_thread_bars`. Tests driving the loop directly must not call `drop_thread_bars`
  (no thread bars were created there) — or create them if they spawn workers (matches `run`).
- Worker loop `Ok(row) => if check_between_files().is_err() { break }`: a graceful request
  causes the worker to **drop the row it just pulled** (never hashed) — populates the
  "some rows still sha1 NULL mid-feed" expectation in test 6.
- The trigger thread must outlive / join before the test asserts (join it).
- Recorded times are `None` unless a test sets them explicitly (only test 3 does).
- If the dialect's test framework needs `#[cfg(test)] mod tests { use super::*; ... }`
  (common/files.rs, flags.rs precedent) vs bare `#[test]` fns (types.rs precedent): use the
  `mod tests` form wherever private fn access is needed.
- Ordering test reads `hash_queue` via a raw query when using the `Database` facade gap
  (`pull_pending_hash_rows` needs an `R: SqlFileRow`; `StrippedRecord` works) — prefer facades
  where they exist, raw `conn.execute`/`query_row` inside `db.with_transaction(|conn| …)` where
  they don't (same-crate tests may use `crate::db::schema::initialize` +
  `rusqlite::Connection::open` for the db module tests instead of `Database`).