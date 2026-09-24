# Plan: Sparsify-phase rewrite (queue + channel loop, hash-style) + unit tests

Status: **settled with the user** (2026-09-24). Rebuild the sparsify phase on the hash-phase
design (ordering table `sparsify_queue`, producer/consumer channel loop, batch ingest), replace
`mark_sparsified_sparse`/`mark_sparsified_error` with a generic `set_file_flag` + `mark_phase`,
introduce a single `SparseOutcome { id, modified, err: Option<Error> }` result struct, keep `rt.*`
at every call site (no `let db = rt.db;` unpacking), use the **dedup-style joined-tail loop**
(owns `mut Vec<JoinHandle<()>>`, joins before `drop(recv)`) and `io_jobs` for the worker pool.
Tests live inline in `#[cfg(test)] mod tests` at the bottom of `archive/sparsify.rs` and
`db/sparsify.rs` (no `tests/` additions) — **test implementation deferred** until the user has
reviewed the production design. No production refactors beyond the accepted redesign.

## Current state of the code (verified 2026-09-24)

`archive/sparsify.rs` (214 lines) — rayon pool over `list_sparsify_candidates`, not resumable:
- `run(rt)`:
  1. page_size>0 guard; `stage_dir = rt.config.paths.stage_dir()`; `create_dir_all`.
  2. `if !rt.config.sparse.sparsify` → `promote_deduped_to_sparsified()` bulk (global inc) and return.
  3. `promote_non_sparsify_candidates_to_sparsified(min_pages)` (global inc).
  4. `candidates: Vec<StrippedRecord> = list_sparsify_candidates(min_pages)` — **TODO batching**;
     `set_phase_total(candidates.len())`; mutex results Vec; rayon pool (`io_jobs`).
  5. Per-record: `warn_if_times_changed` via `PreYield`, `sparse_member_name()` (+ `.clean()`
     dst under stage_dir, `TempSparseFile` for drop-delete), `sparse_copy_with_progress(
     abs_path, tmp, page_size, |..| is_force()? Err(Interrupted):Ok)`, push `SparseOutcome::Ok/Err`.
  6. Drain: per outcome `mark_sparsified_sparse(id)` / `mark_sparsified_error(id)` +
     `recorder.record_file(id, ERROR_PHASE, e, ErrorFlags::default())`; Interrupted dropped; other → panic.
  7. Tail: `match parallel { Ok → sanity_no_deduped + info; Interrupted → warn(saved); else e }`.
- `SparseOutcome` enum `{ Ok(FileId), Err(FileId, Error) }` (worker-internal), `TempSparseFile`
  (keep/drop-delete), `run_pool`, `sanity_no_deduped(db)`.

`db/sparsify.rs` (96 lines):
- `promote_deduped_to_sparsified(conn) -> u64` — `UPDATE … SET phase='sparsified' WHERE phase='deduped'`.
- `promote_non_sparsify_candidates_to_sparsified(conn, min_pages) -> u64` — the negation of the
  candidate predicate (canonical_id IS NULL/NOT-self/!file/sha1 NULL/low sparse_count/HasSparse/filtered-out).
- `list_sparsify_candidates<R>(conn, min_pages) -> Vec<R>` — `WHERE phase='deduped' AND canonical_id=id
  AND ftype='file' AND sparse_count>=:min_pages AND (flags&:has_sparse)=0 AND {generate_archive_filter(None)}
  ORDER BY size DESC`.
- `mark_sparsified_sparse(conn, id)` — phase→sparsified + `flags |= HasSparse`.
- `mark_sparsified_error(conn, id)` — phase→sparsified + `flags |= ErrorWhileSparsify`.

Supporting facts (verified):
- `FileFlag::HasSparse=9`, `FileFlag::ErrorWhileSparsify=10`, `FileFlag::Modified=5` (`db/flags.rs`).
- `flags::set_file_flag(conn, FileId, FileFlag, on) -> Result<u64>` (rows affected, CASE update);
  `common::mark_phase(conn, FileId, FilePhase) -> Result<()>`.
- `Error: From<io::Error>` (error.rs:131) → `sparse_copy_with_progress` infers `E=Error`.
- `SparseCopyStats { size_in, size_out, bytes_saved, zero_blocks }`; success value unused by us.
- `db.rs` facades: `promote_deduped_to_sparsified`, `promote_non_sparsify_candidates_to_sparsified`,
  `list_sparsify_candidates`, `mark_sparsified_sparse`, `mark_sparsified_error` (:459–:477).
- hash plan/tests precedent: `hash_queue` table + `create/populate/pull_pending_hash_rows/drop`;
  `handle_send_receive_loop`; 10_000/20_000/1024/5_000 capacity constants; worker sends
  `Some(outcome)` per row + `None` at exit (`bounded::<Option<…>>`); `drain_chunk` with
  `Ok(None) → *exit += 1`.
- dedup tests precedent: TestWorld seeds `INSERT OR IGNORE INTO filter_reason_archive (id, source,
  line, expression) VALUES (-1,'internal',NULL,'.*')` before `apply_no_filter()` (FK);
  `ProgressBarSet::new(ARCHIVE_MULTIPLIER)`, `Shutdown::detached()`, `world.rt()`, `run_loop`
  helper pushing the `JoinHandle` into a `mut handles` Vec, `thread::join()`/`trigger.join()`.
- `pull_pending_hash_rows` returns `(queue_pos, R)` pairs; feed advances `queue_index` from the
  last returned position; slice re-scan restarts at that index across refills.

## Design

### `db/sparsify.rs`

Outcome struct (mirrors `HashingOutcome`'s payload but single struct per user spec):

```rust
pub struct SparseOutcome {
    pub id: FileId,
    pub modified: bool,
    pub err: Option<Error>,
}
// None = success; Some(Error::FileStat(_)) = per-file failure (record + ErrorWhileSparsify);
// Some(Error::Interrupted) = aborted in-flight (discard, row stays deduped).
```

Keep unchanged: `promote_deduped_to_sparsified`, `promote_non_sparsify_candidates_to_sparsified`.

Remove: `list_sparsify_candidates`, `mark_sparsified_sparse`, `mark_sparsified_error`.

Add (hash-queue mirror, candidate predicate = the old `list_sparsify_candidates` WHERE):

- `sparsify_candidates_where() -> String` — shared fragment (params `:min_pages`, `:has_sparse`):
  `phase = 'deduped' AND canonical_id = id AND ftype = 'file' AND sparse_count >= :min_pages
   AND (flags & :has_sparse) = 0 AND {generate_archive_filter(None)}`
- `create_sparsify_queue(conn) -> Result<()>` —
  `CREATE TABLE IF NOT EXISTS sparsify_queue (id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL UNIQUE REFERENCES files(id))` (idempotent).
- `populate_sparsify_queue(conn, min_pages) -> Result<u64>` — `INSERT OR IGNORE INTO
  sparsify_queue (id, file_id) SELECT row_number() OVER (ORDER BY size DESC, id), id FROM files
  WHERE {sparsify_candidates_where()}` (params `:min_pages`, `:has_sparse`).
- `pull_pending_sparsify_rows<R: SqlFileRow>(conn, index: u64, limit: u64) -> Result<Vec<(u64, R)>>` —
  `SELECT sparsify_queue.id AS pos, {cols} FROM files JOIN sparsify_queue ON
  sparsify_queue.file_id = files.id WHERE sparsify_queue.id > :index AND files.phase = 'deduped'
  AND (files.flags & :error_flag) = 0 ORDER BY sparsify_queue.id LIMIT :limit`
  (`:error_flag = FileFlag::ErrorWhileSparsify.mask_i64()`; same row-mapping shape as `pull_pending_hash_rows`).
- `drop_sparsify_queue(conn) -> Result<()>` — `DROP TABLE IF EXISTS sparsify_queue` (success path only).
- `count_pending_sparsify_candidates(conn, min_pages) -> Result<u64>` —
  `SELECT COUNT(*) FROM files WHERE {sparsify_candidates_where()} AND (flags & :error_flag) = 0`.
  (Errored rows leave `deduped` at ingest, so phase='deduped' already excludes them; the extra
  excludes the sticky-flag edge like `pull` does.)
- `ingest_sparsify_outcome(conn: &mut Connection, results: &Vec<SparseOutcome>) -> Result<u64>` —
  one transaction; per outcome:
  - `if outcome.modified: assert_eq!(1, set_file_flag(&tx, id, FileFlag::Modified, true)?);`
  - `match &outcome.err`:
    - `None` → `assert_eq!(1, set_file_flag(&tx, id, FileFlag::HasSparse, true)?);`
      `mark_phase(&tx, id, FilePhase::Sparsified)?; resolved += 1;`
    - `Some(Error::FileStat(_))` → same with `FileFlag::ErrorWhileSparsify`; `resolved += 1;`
    - `Some(Error::Interrupted)` → `()` (row untouched)
    - `other` → `panic!("Invariant Error. Only FileStatError and Interrupted expected. Got: {other}")`
  - `tx.commit()?; Ok(resolved)`.

`use` additions: `crate::db::common::{.., mark_phase}`, `crate::db::flags::{FileFlag, set_file_flag}`,
`crate::db::types::FilePhase`.

### `db.rs` facades
- Remove: `list_sparsify_candidates`, `mark_sparsified_sparse`, `mark_sparsified_error`.
- Add: `create_sparsify_queue()`, `populate_sparsify_queue(min_pages)`,
  `pull_pending_sparsify_rows<R>(index, limit)`, `drop_sparsify_queue()`,
  `count_pending_sparsify_candidates(min_pages)`, `ingest_sparsify_outcome(&Vec<SparseOutcome>)`
  (`sparsify::ingest_sparsify_outcome(&mut self.conn_mut(), &results)` like the hash facade).
- Add `use crate::db::sparsify::SparseOutcome;` next to the `CompareOutcome`/`HashingOutcome` imports.

### `archive/sparsify.rs`

Imports: drop `PreYield`/`rayon`/`Mutex`; keep `path_clean::PathClean`, `std::fs`, `TempSparseFile`,
`ERROR_PHASE`; add `crate::common::at_least_one_running`, `crate::db::sparsify::SparseOutcome`,
`crossbeam_channel::{bounded, Receiver, Sender}`, `indicatif::ProgressBar`, `std::mem::take`,
`std::thread`, `std::time::Duration`, `crate::progress::BarKind`.

Constants (copy hash): `BATCH_SIZE = 10_000`, `WORK_CAPACITY = BATCH_SIZE`,
`OUT_CAPACITY = 2 * BATCH_SIZE`, `FEED_CHUNK = 1024`, `DRAIN_CHUNK = BATCH_SIZE / 2`.

`run(rt: &ArchiveRTArgs) -> Result<()>` — `rt.*` at every call site:

1. page_size>0 guard + `tracing::info!` (unchanged wording).
2. `let stage_dir = rt.config.paths.stage_dir();` `create_dir_all` (io-map on failure).
3. `if !rt.config.sparse.sparsify` → `promote_deduped_to_sparsified()`, `inc_global(n)`,
   `rt.db.drop_sparsify_queue()?` (stale queue from an interrupted run), info, return.
4. `skipped = rt.db.promote_non_sparsify_candidates_to_sparsified(min_pages)?; inc_global(skipped); info.`
5. `rt.db.create_sparsify_queue()?; rt.db.populate_sparsify_queue(min_pages)?;`
   `pending = rt.db.count_pending_sparsify_candidates(min_pages)?;`
   `rt.progress.set_phase_total(pending);` if 0 → `drop_sparsify_queue`, `sanity_no_deduped(rt.db)?`, return.
6. `jobs = rt.config.process.io_jobs;` `rt.progress.create_thread_bars(BarKind::Bytes, jobs);`
   materialize `Vec<ProgressBar>` via `rt.progress.thread_bar(i)`.
7. Channels `bounded::<StrippedRecord>(WORK_CAPACITY)` + `bounded::<Option<SparseOutcome>>(OUT_CAPACITY)`;
   spawn `jobs` workers (`thread::Builder` name `sparsify-worker-{i}`, clones bar/work_r/out_s/shutdown/stage_dir/page_size),
   push every handle into `mut thread_handles`; `drop(work_r); drop(out_s);`.
8. `let (completed, errored) = run_enqueue_dequeue_loop_sparsify(&rt, work_s, out_r, thread_handles)?;`
   `rt.progress.drop_thread_bars();`
9. Tail `match rt.shutdown.is_interrupted()`: `true` → warn(`saved = completed`) + `Err(Error::Interrupted)`
   (graceful/force msg split like hash/dedup); `false` → `drop_sparsify_queue()`, `sanity_no_deduped(rt.db)?`,
   `tracing::info!(ok = completed - errored, err = errored, "sparsify complete")`, `Ok(())`.

`run_enqueue_dequeue_loop_sparsify(rt, send: Sender<StrippedRecord>, recv: Receiver<Option<SparseOutcome>>,
mut handles: Vec<thread::JoinHandle<()>>) -> Result<(u64, u64)>` (dedup-style owned handles):

- state: `recorder`, `queue_index: u64`, `feed_buf: Vec<(u64, StrippedRecord)>`, `feed_idx`,
  `feed_exhausted`, `busy`, `dequeue_total`, `exited_workers`, `feed_total`, `completed`, `errored`,
  `pending_out: Vec<SparseOutcome>`.
- `apply_chunk |pending| -> Result<(u64, u64)>`: `items = take(pending)`; empty → `Ok((0,0))`;
  `n = items.len() as u64`; `rt.db.ingest_sparsify_outcome(&items)?`; per item `match &res.err`:
  `None → ()`, `Error::Interrupted → ()`, `Error::FileStat(_) → record_sparsify_error(&mut recorder, &res); err_i += 1`,
  `other → panic!(…)`; `rt.progress.inc_both(n)`; `Ok((n, err_i))`.
- `drain_chunk |is_busy, drain_override, dequeue, exit|`: pull `Some(res)` → push/busy/dequeue,
  `Ok(None)` → `*exit += 1`, `Err` → break; apply when `len >= DRAIN_CHUNK || override`
  (`(n, e) = apply_chunk(&mut pending_out)?; completed += n; errored += e; *is_busy = true`).
- **Feed loop**: `if rt.shutdown.is_interrupted() break; busy=false;` refill `pull_pending_sparsify_rows::<StrippedRecord>(queue_index, FEED_CHUNK as u64)` when `feed_idx == feed_buf.len()`
  (empty → `feed_exhausted = true`, else `queue_index = last.0`); `send.try_send(row.clone())` until
  full (`feed_total += 1; busy = true; feed_idx += 1`); `drain_chunk(busy, false, …)?`;
  break when `feed_exhausted && feed_idx == feed_buf.len()`; break when `!at_least_one_running(&handles.iter().collect())`;
  `if !busy sleep(10ms)`.
- `drop(send);`
- **Drain tail** (mirror hash): loop { `if dequeue_total == feed_total break;`
  `if !at_least_one_running(&handles.iter().collect()) break;`
  `if exited_workers == rt.config.process.io_jobs as u64 break;`
  `drain_chunk(busy, false, …)?; if !busy sleep(4ms)` }.
- **Join tail (I3 style)**: `while !handles.is_empty() { take(&mut handles).join().expect("join sparsify worker"); }`
  (workers exit promptly because `send` was dropped; joining before `drop(recv)` makes the trailing
  `out.send(None)` race impossible).
- final `drain_chunk(busy, true, …)?; drop(recv); recorder.flush()?; Ok((completed, errored))`.

`sparsify_worker(bar: ProgressBar, stage_dir: PathBuf, page_size: usize, shutdown: Shutdown,
work: Receiver<StrippedRecord>, out: Sender<Option<SparseOutcome>>) -> ()`:

```
loop match work.recv() {
    Ok(row) => {
        if shutdown.check_between_files().is_err() { break; }
        bar.reset(); bar.set_length(row.size);
        bar.set_message(format!("Sparsifying {}", row.abs_path.display()));
        let modified = warn_if_times_changed(&row.abs_path, row.mtime, row.atime, row.ctime);
        let name = row.sparse_member_name().expect("Invariant: sparsify candidates must be self-canonical files");
        let dst = stage_dir.join(name).clean();
        let tmp = TempSparseFile::new(dst);
        let outcome = match sparse_one(&row.abs_path, tmp.path(), page_size, &shutdown, Some(&bar)) {
            Ok(_) => { tmp.keep(); SparseOutcome { id: row.id, modified, err: None } }
            Err(err) => SparseOutcome { id: row.id, modified, err: Some(err) },
        };
        out.send(Some(outcome)).expect("sparsify worker: result channel closed");
    }
    Err(_) => break,
}
out.send(None).expect("sparsify worker: result channel closed");
```

`sparse_one(path: &Path, dst: &Path, page_size: usize, shutdown: &Shutdown, pb: Option<&ProgressBar>)
-> Result<SparseCopyStats>` (mirror `hash_one` + the old copy callback):

```rust
let mut last = 0u64;
sparse_copy_with_progress(path, dst, page_size, |pos, _size, _dur| {
    if let Some(pb) = pb {
        let delta = pos.saturating_sub(last);
        if delta > 0 { pb.inc(delta); }
        last = pos;
    }
    if shutdown.is_force() {
        Err(Error::Interrupted)
    } else {
        Ok(())
    }
})
```
(graceful lets the in-flight copy finish, force aborts via callback; aborted partial temp
deleted by `TempSparseFile` drop; `E = Error` via `From<io::Error>`.)

`record_sparsify_error(recorder: &mut Recorder, o: &SparseOutcome)` — `recorder.record_file(o.id,
ERROR_PHASE, o.err.expect("record_sparsify_error: err must be Some").to_file_stat(None),
ErrorFlags::default());` (only invoked when `err` is `Some(Error::FileStat(_))`; parity with the old
empty-path `FileStatError::Io` — `From::from(io::Error)` cannot carry the path. Do **not** fix the
path here; note the pre-existing limitation.)

`sanity_no_deduped(db: &Database) -> Result<()>` — unchanged.

## Tests (planned — NOT implemented until user review)

### `db/sparsify.rs` (`mod tests`, raw `Connection` + `schema::initialize`)
Fixed literal masks `FLAG_HAS_SPARSE = 1<<9`, `FLAG_ERR_SPARSIFY = 1<<10`, `FLAG_MODIFIED = 1<<5`
(const-callable, like db/hash tests). Fixtures seed the `-1` internal include rule
(`INSERT OR IGNORE INTO filter_reason_archive …`) and rows with `include_reason_archive = -1`,
`exclude_reason_archive = 0`, `ftype='file'`, `canonical_id=id`, `phase='deduped'`,
`sparse_count = N`, `size = N` (helper `insert_candidate(conn, id, size, sparse_count)`).

1. `ingest_ok_sets_has_sparse_and_promotes_phase` — `err: None` → HasSparse set, phase `sparsified`, resolved 1.
2. `ingest_filestat_error_sets_error_flag_and_promotes_phase` — `Some(Error::FileStat(::General))`
   → ErrorWhileSparsify set, phase `sparsified`, resolved 1.
3. `ingest_interrupted_leaves_row_untouched` — flags 0, phase `deduped`, resolved 0.
4. `ingest_sets_modified_flag_on_ok_and_err` — both arms set Modified.
5. `ingest_panics_on_invalid_error_variants` — `catch_unwind(AssertUnwindSafe(…))` over
   `Error::Config/Other/Database` (mirror db/hash).
6. `queue_populate_orders_by_size_desc` — 3 candidates sizes 8MiB/4MiB+1/1MiB →
   `pull_pending_sparsify_rows::<StrippedRecord>(0, 100)` → ids descending; positions monotone.
7. `pull_skips_promoted_and_errored_rows` — one row pre-`sparsified`, one with ErrorWhileSparsify
   flag, one pending → only the pending row returned; slice walk via returned pos reproduces the full list.
8. `count_pending_matches_pull` — count == length of full pull; sub-min-pages rows not counted.
9. `populate_is_idempotent` — run twice → same full pull (INSERT OR IGNORE).
10. `promote_non_sparsify_candidates_promotes_but_keeps_candidates` — candidate stays `deduped` in
    the queue set; dup-canonical / sub-min-pages / HasSparse / non-file rows → `sparsified` (pin).

### `archive/sparsify.rs` (`mod tests`, TestWorld mirroring hash/dedup)
TestWorld: tempdir + `Database::open` + `apply_no_filter` FK seed (`filter_reason_archive` -1) +
`Shutdown::detached()` + `ProgressBarSet::new(ARCHIVE_MULTIPLIER)` + `test_archive_config()`
with `sparse: SparseOptions { sparsify: true, page_size: 4096, min_pages: 4 }`, `io_jobs: 2`.
Helpers: `add_file`, `path`, `rt()` (+ `rt_with(shutdown)`), `flag(id, FileFlag)`, `phase(id)`,
`seed_dedup_row(id, sparse_count)` (tx: `SET phase='deduped', canonical_id=id, sparse_count=:n`),
`prepare_for_sparsify()` (promote-non-candidates + `create/populate_sparsify_queue`, mirroring
dedup's `prepare_for_loop`), `run_loop(world, bar, work_cap)` pushing the worker handle into
`mut handles` and calling `run_enqueue_dequeue_loop_sparsify` directly.

Payloads: zeros (`vec![0u8; n]`) so the sparse rewrites are genuinely hole-y; ≤ 16 MiB files,
few per test (disk-headroom constraint).

1. `sparse_one_writes_sparse_destination` — 4 pages of zeros → dst exists, `metadata(dst).len() ==
   size`, `metadata(dst).blocks() * 512 < size` (hole-y), stats.bytes_saved > 0.
2. `sparse_one_error_is_filestat` — chmod 000 source (`geteuid().is_root()` skip) → `Err(Error::FileStat(_))`.
3. `sparse_one_force_interrupts_mid_copy` — force pre-set → `Err(Error::Interrupted)`, dst absent (drop-delete).
4. `sparsify_worker_sends_none_on_graceful_exit` — row queued + graceful pre-set → only `None`
   on recv, no outcome (worker breaks between files).
5. `sparsify_worker_outcome_success` — one candidate → `Some(outcome)` with `id`, `modified=false`, `err=None`.
6. `run_sparsifies_candidates_and_flags` — 3 candidates (0x·/sparse_count 8/4/1) + 1 non-candidate
   (sub-min sparse_count): after `run` success — candidates phase `sparsified` + HasSparse, `sp.{content_id}`
   exists in work dir for the self-canonical ones; non-candidate promoted, no HasSparse, no sp file;
   queue dropped (pull errors / table gone via count).
7. `run_with_sparsify_disabled_promotes_all` — config `sparsify: false` → all `deduped` → `sparsified`, no flag.
8. `unreadable_file_recorded_not_fatal` — chmod 000 candidate (root-skip) → ErrorWhileSparsify +
   phase `sparsified` + one error record (ErrorPhase::Sparsify), siblings clean.
9. `modified_file_flagged` — stale mtime (`Utc::now()-3600`) → Modified set.
10. `queue_order_is_size_desc` — populate then `pull_pending_sparsify_rows::<StrippedRecord>(0,100)`
    → ids in `size DESC` order.
11. `loop_applies_partial_batch_no_loss` — 3 candidates, `run_loop(bar, work_cap=2)` → completed 3,
    all flagged; final drain covers the ragged tail.
12. `interrupt_mid_feed_pauses_and_resume_completes` — 8×16 MiB candidates, trigger thread
    (bar `position() > 0` then `== 0` → `request_graceful`), loop → `1 <= completed < 8`,
    pending == 8-completed, queue alive; resume with fresh `Shutdown::detached()` → `8` total,
    all flagged.
13. `interrupt_dequeue_only_finishes_in_flight` — cap-2 channel, 3 files, graceful after first
    completion (steady-state drain) → completed ≥ 1 cliff, resume tops up.
14. `force_aborts_in_flight_discards` — force mid-copy → outcome err `Some(Interrupted)`; row stays
    `deduped`, no flags, no error records, no sp file; resume with fresh shutdown completes it.
15. `run_force_before_start_returns_interrupted` — `request_force()` before `run` → `Err(Error::Interrupted)`,
    no row touched, queue survives.
16. `record_sparsify_error_persists_filestat` — direct `record_sparsify_error` + flush →
    record row has file_id, `error_type == "Io/PermissionDenied"`,
    `phase == Pipeline(Sparsify)`.

## Verification
- `timeout 900 nix develop --command cargo build -p tar-dedup -p tar-dedup-cli` (prod green;
  expect only the pre-existing warnings).
- `timeout 900 nix develop --command cargo test -p tar-dedup --lib -- 'sparsify'` (db + archive suite —
  after tests are implemented).
- `timeout 1500 nix develop --command cargo test -p tar-dedup --lib` — expect the same 4 pre-existing
  failures (inventory ×2, scan ×2) and 1 ignored hash stub; everything else green.
- Never `--release` for tests; keep test files ≤ 16 MiB.

## Notes / decision log
- Queue survives interrupts (dropped on success only), mirroring `hash_queue`; a stale queue from a
  previous mode is dropped on the `sparsify=false` skip path.
- Output `SparseCopyStats` unused on success (`tmp.keep()` is the only side effect we need).
- `SparseOutcome` carries `Option<Error>` (not `Option<FileStatError>`) so in-flight force aborts are
  representable and `ingest` can discard them — same role as hash's `HashError.err`.
- Recording error paths keep the old empty-path `FileStatError::Io` (a `From::from(io::Error)`
  limitation already present today) — not in scope to fix.
- `io_jobs` for worker count (config docs: "I/O-bound workers (sparsify, dedup, place pools)"),
  not `effective_jobs()`.
- The user may later want the hash loop re-synced to the same joined-tail shape; sparsify uses it
  from the start.
- Result channel = `bounded::<Option<SparseOutcome>>(OUT_CAPACITY)`; `None` on it is the per-worker
  "thread exited" marker (hash/dedup trick), counted by `drain_chunk`'s `Ok(None) → *exit += 1`.