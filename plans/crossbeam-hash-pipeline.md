# Plan: crossbeam producer/consumer hash phase (replaces batched_loop in hash.rs)

## Context / why

The hash phase currently pulls up to `BATCH_SIZE` (= 10_000) pending files at once via
`batched_loop` + `get_entries_to_hash` (`ORDER BY size DESC LIMIT :n`, no cursor), runs a rayon
`pool.install` over them, and applies all results in end-of-batch. Two problems:

1. **Big-file barrier**: a single large file (e.g. 50 GiB) inside a batch dominates the batch's
   wall time; no result is committed and no progress advances until the *entire batch* finishes.
   With the biggest file first (size DESC), the 9999 next-largest files finish far earlier and
   their workers idle.
2. **Batching exists to bound memory** (only ~10k rows in RAM). Removing the batch to fix (1)
   would re-open unbounded memory, so the fix must keep a memory bound *while* streaming work.

**Decision (user-approved):** a producer/consumer pipeline on **crossbeam** channels. Workers pull
individual files from a **bounded input queue** (memory bound), push results to an **output queue**;
the main thread feeds the input from a lazy DB iterator and drains/commits the output in small
transactions. This keeps the 50 GiB file on one worker while the others stream the rest, commits +
progress advance continuously, and memory stays bounded.

## Non-goals for this round (recorded, do NOT do)

- **Cursor-facade DB rewrite (later):** exposing a Python-style `Connection`+multiple `Statement`
  ("cursor") API on `Database`. Rationale documented below. For THIS round keep `&self` /
  `&Connection` signatures everywhere.
- **Shared `common` pipe module** for dedup/sparsify/place to reuse later (keep the pipeline
  inline in `hash.rs` so the worker's business logic stays individually testable).
- **CLI arg for batch size** (future; today keep the `BATCH_SIZE` const in `hash.rs`).
- **Full integration test suite for the hash phase** (later). Only *small single-function* tests
  are allowed now (e.g. a pure `hash_one` case); no integration tests, no channel-pipeline tests.
- No DB schema changes. No removal of `common::batched_loop` / `common::batched_stepped_loop`
  (keep them untouched). `get_entries_to_hash` stays (may become unused by hash.rs).

## Repo state / starting point (read before touching code)

- Working tree has previously-made **uncommitted** changes across
  `crates/tar-dedup/src/{archive.rs, db.rs, common.rs, progress.rs, archive/hash.rs}` plus the
  user's own WIP in `archive/dedup.rs, db.rs, db/dedup.rs`. `git stash list` is empty.
- `crates/tar-dedup/src/archive/hash.rs` at `HEAD` (`18ff355`) is the **user's committed rework**
  (batched_loop + PreYield + rayon install + results Mutex + end-of-batch apply). The pipeline
  below REPLACES that rework's worker/drain structure. Prefer to implement on the committed
  structure: `git checkout -- crates/tar-dedup/src/archive/hash.rs` is acceptable so the diff is
  clean, but check with the user first if unsure.
- Keep `hash_file`'s core hashing/zero-page logic; only its signature changes (→ `hash_one`).
- Keep `IdError`, `record_hash_error`, eager-filter/hardlink branches, recorder, resume semantics.

## Why a second connection for the feeder (SQLite cursor constraint)

rusqlite `Connection` == Python `Connection`; rusqlite `Statement` (via `conn.prepare` /
`conn.stmt`) == Python `cursor`. SQLite allows many prepared statements per connection, but only
**one** may be actively `sqlite3_step`-ing on that connection at a time. Therefore a long-lived
read cursor cannot coexist with writes on the SAME connection. The plan opens a **second
`Database::open(db_path)`** purely for the streaming read; WAL lets the reader's snapshot and the
writer's commits coexist. This is the Rust/SQLite equivalent of Python's "open a second
`connect()` to read while I write" — the cursor-facade rewrite would only change *which handle*
provides the statement, not the mechanism.

## Constants (in `hash.rs`, `BATCH_SIZE` stays the declared global const)

```rust
const BATCH_SIZE: usize = 10_000;      // exists; input capacity + the app's future CLI knob
const WORK_CAPACITY = BATCH_SIZE;      // input queue bound = the memory guard (replaces batch)
const OUT_CAPACITY  = 2 * BATCH_SIZE;  // output queue larger than input (user: halves/doubles)
const FEED_CHUNK    = 1_024;           // rows pulled per round from the DB iterator
const DRAIN_CHUNK   = BATCH_SIZE / 2;  // 5_000 outcomes per commit (one tx per chunk)
```

## Data flow

```
DB (second connection)                          MAIN THREAD                         
  hash_rows_stream(eager, hardlinks) ─┐    feed: pull FEED_CHUNK rows → try_send     ┌──────────────┐
  lazy MappedRows iterator (size DESC,└──▶  work: CrossbeamChannel<StrippedRecord> ─▶│ Worker idx   │
  one row exactly once per snapshot)          capacity WORK_CAPACITY                 │ bars[idx] by │
                                                                                     │ value only   │
  drain: try_recv up to DRAIN_CHUNK ◀─── out: CrossbeamChannel<Outcome>  ◀──────────│ local buf    │
  apply via with_transaction (write conn)     capacity OUT_CAPACITY                  │ (Vec<u8>)    │
  progress.inc_both(n); idle ~1ms             (blocking send, main drains ⇒          │ for row in   │
  terminate when iterator exhausted AND       no deadlock)                           │ recv_iterator│
  completed == fed → send_close → join workers                                       └──────────────┘
```

## Worker (threading wrapper; per-user points 3+4)

Spawn **`effective_jobs()`** plain `std::thread`s (NOT rayon) — the whole program may move to
crossbeam-style later. Each worker:

```rust
fn hash_worker(
    bar: ProgressBar,               // passed BY VALUE at spawn; never holds ProgressBarSet
    page_size: usize,
    shutdown: &Shutdown,
    work: CrossbeamChannel<StrippedRecord>,
    out:  CrossbeamChannel<std::result::Result<(FileId, [u8; 20], u64), IdError>>,
) {
    let mut buf = Vec::new();       // per-worker read buffer, sized once, reused across files
    for row in work.recv_iterator() {
        if shutdown.check_between_files().is_err() { break }
        bar.reset();
        bar.set_message(format!("Hashing {}", row.abs_path.file_name()));  // + set_length(size)
        let res = hash_one(&mut buf, &row.abs_path, page_size, &shutdown, Some(&bar))
                      .map(|(d, zb)| (row.id, d, zb))
                      .map_err(|e| IdError { err: e, id: row.id });
        out.send(res);              // outcome; completed_files counting
    }
}
```

- Bar handles: materialize all bars **before** spawning via
  `progress.create_thread_bars(BarKind::Bytes, effective_jobs())` then `thread_bar(0..n-1)`;
  hand `bars[idx]` by value to worker `idx`. Workers then never touch `ProgressBarSet` ⇒ no set
  locks in the hot path; all worker vars are thread-local (bar, buf, etc.).
- Workers have full `tracing:` access (global). They do **not** touch the DB.

## `hash_one` — the ONLY change to the hashing logic

Signature (user-confirmed option a; buffer comes first, created in the worker thread):

```rust
fn hash_one(buf: &mut Vec<u8>, path: &Path, page_size: usize,
            shutdown: &Shutdown, pb: Option<&ProgressBar>) -> Result<([u8; 20], u64)>
```

- `buf` is the worker's local; `hash_one` sizes it to `IO_BUF_SIZE` once (first call) and reuses
  it (no per-file 4 MiB allocation — that was a measured dominant cost; per-file allocation +
  zero-fill scaled worse with threads).
- Keep the read loop + zero-page logic of the committed `hash_file` verbatim; add
  `if let Some(pb) = pb { ... }` per-read-buffer `pb.inc(n as u64)` so a 50 GiB file's bar moves
  live (this is why the bar is inside the read loop, not just set once by the worker).
- Needs `IO_BUF_SIZE` accessible → make `crate::common::IO_BUF_SIZE` `pub` (THE single common.rs
  change). Keep `io_buffer()` as-is (still used elsewhere).

## Main loop (feeder + drainer; one function in run())

```rust
work = CrossbeamChannel::bounded::<StrippedRecord>(WORK_CAPACITY);
out  = CrossbeamChannel::bounded::<Outcome>(OUT_CAPACITY);
let read_db = Database::open(db_path);                 // second connection
let mut rows = read_db.hash_rows_stream(eager, hardlinks);  // lazy iterator (main-only)
spawn workers; let fed = AtomicU64 / local counter; completed = 0;
pending_out: Vec<Outcome> = Vec::new();
loop {
    if shutdown.check_between_files().is_err() { interrupted = true; break }
    // drain
    while pending_out.len() < DRAIN_CHUNK {
        match out.try_recv() { Some(r) => pending_out.push(r), None => break }
    }
    if pending_out.len() >= DRAIN_CHUNK || (feed_done && out empty) {
        apply_chunk(&pending_out); completed += applied;
    }
    // feed
    if !feed_done {
        for _ in 0..FEED_CHUNK {
            match rows.next() {
                Some(row) => match work.try_send(row) {
                    Ok(_) => fed += 1, Full => break
                },
                None => { feed_done = true; break }
            }
        }
    }
    if feed_done && completed == fed && pending_out.is_empty() { break }
    if nothing happened this round { thread::sleep(1ms) }   // main idles
}
work.send_close(); join all workers;
// ---- cleanup fn ----
// definitive drain: out.recv() until channel reports closed/empty (blocking);
// push into pending_out; apply remaining in one tx; drop_thread_bars();
if interrupted { warn(stopped/force-aborted); return Err(Error::Interrupted) }
recorder.flush()?; double-canonical check; Ok(())
```

- **Termination**: iterator yields each pending row exactly once (snapshot); when exhausted, all
  rows are either applied or in a worker's hand. `completed == fed` ⇒ every handed-out row has an
  outcome ⇒ workers are blocked on `recv` of an empty/closed channel ⇒ `send_close` + join return
  immediately. `pending_out` must be applied (or retained) before the count check so the loop
  doesn't spin.
- **Interrupt policy (unchanged from today)**: single Ctrl-C = graceful → workers finish their
  current file (yes, the 50 GiB one; it will be re-hashed on resume) then exit; completed
  outcomes are applied + saved; in-flight result discarded (row is still `sha1 IS NULL` so resume
  re-picks it). Force abort (2nd signal) → `check_in_flight` inside `hash_one` bails fast,
  discarding in-flight bytes. Warnings to match current messages
  ("hashing stopped; completed files saved" / "hashing force-aborted; in-flight progress discarded").
- **apply_chunk** = `db.with_transaction(|tx| for res in chunk { match res {
  Ok(id,digest,zb) => crate::db::hash::update_file_inspection_per_id(tx, id, digest, zb, hardlinks)?,
  Err(e) => crate::db::flags::set_file_flag(tx, e.id, ErrorWhileHash, true) + record_hash_error(recorder,&e) }})?`,
  then `progress.inc_both(chunk.len())` (global counter advanced on the main thread as files leave
  the phase — the "+50" semantics).

## DB additions (additive only)

- `crates/tar-dedup/src/db/hash.rs`:
  `pub fn hash_rows_stream(conn: &Connection, eager: bool, detect_hardlinks: bool)
   -> Result<impl Iterator<Item = StrippedRecord>>` — same `SELECT ... WHERE ... ORDER BY size DESC`
   as `get_entries_to_hash` but **without LIMIT**, returned as a lazy rusqlite `MappedRows`
   iterator (`prepare → query_map(params, |r| R::from_row(r, None))`). Do NOT change
   `get_entries_to_hash`.
- `Database::with_transaction` (already added earlier, keep) + `pub(crate) mod hash;` (already
  added earlier, keep) so hash.rs can call the low-level `crate::db::hash::update_file_inspection_per_id(tx, …)`.
- Second connection: `Database::open(db_path)` in `hash::run` for the reader only (main thread).
  Both connections WAL; the reader holds a snapshot for the phase (writer WAL not checkpointed
  until the iterator drops — negligible).

## File-by-file change list

| File | Change |
|---|---|
| `crates/tar-dedup/Cargo.toml` + `Cargo.toml` (workspace) | add `crossbeam = "0.9"` (network fetch in nix shell; MPMC `CrossbeamChannel`) |
| `crates/tar-dedup/src/common.rs` | ONLY: `const IO_BUF_SIZE` → `pub const IO_BUF_SIZE`; `batched_loop`/`batched_stepped_loop`/`io_buffer` untouched |
| `crates/tar-dedup/src/db.rs` | keep `with_transaction` + `pub(crate) mod hash;` (from earlier) |
| `crates/tar-dedup/src/db/hash.rs` | add `hash_rows_stream` (lazy, no LIMIT); existing fns untouched |
| `crates/tar-dedup/src/archive/hash.rs` | the pipeline: `hash_one`, `hash_worker`, main feed/drain loop + cleanup; remove `batched_loop`/`PreYield`/rayon `pool.install`/`results Mutex` usage; keep `IdError`/`record_hash_error`/`promote_unhasheable_files`/`count_*`/hardlink+eager branches; add second-connection reader + thread-bar wiring + drop_thread_bars |

## crossbeam API (verify against the fetched 0.9 crate at implementation time)

`crossbeam::channel::crossbeam_channel::CrossbeamChannel::bounded::<T>(cap)`,
`try_send(v)` (→ `Ok`/`Full` …), `recv()` (blocking), `try_recv()`,
`recv_iterator()` (ends after `send_close`), `send_close()`. Workers use `recv_iterator` /
blocking `recv`; main uses `try_send`/`try_recv` only (never blocks ⇒ no deadlock with bounded
queues + a single main actor).

## Verification (after implementation; no new test suite)

1. `cargo build -p tar-dedup-cli` (note: user's `dedup.rs` WIP may block; coordinate).
2. Smoke: archive a 10k-file tree (`--no-hardlink-detection --lazy-filter --jobs 16`) — full run
   exits 0; hash phase completes; DB consistent (dedup sanity passes).
3. Big-file streaming: add a large file with small files around it; confirm progress/global
   advances and DB commits while the large file is still hashing (watch the thread bar).
4. Interrupt mid-hash (graceful + force): state saved; completed files applied; resume completes.
5. Piped run: no control codes on stdout; INFO/WARN on stdout, ERROR on stderr (unchanged).
6. Optional single-function test allowed: `hash_one` pure cases only (bad path, zero-pages). No
   integration/channel tests.

## Follow-ups (NOT this round — recorded only)

- Cursor-facade `Database` rewrite (Connection + multiple `Statement`s) as the "Python cursor"
  structure — separate, later.
- Shared pipe module in `common` for dedup/sparsify/place adoption (whole program → crossbeam).
- CLI arg for `BATCH_SIZE`.
- Full hash-phase integration test suite + per-thread progress cleanup if the thread-bar pool
  proves clumsy.
- Colored stdout logs (ANSI-safe) — deferred from the multiprogress round.

## Implementation notes (as-built, Sep 22)

Verified deviations from the plan's letter, all within its intent:

- **Dependency is `crossbeam-channel = "0.5.17"`, not `crossbeam 0.9`.** The umbrella crate tops
  out at 0.8.5; from 0.9 the project publishes split sub-crates, and channels live in
  `crossbeam-channel`. API difference: **there is no `send_close()` or `recv_iterator()`.**
  `bounded::<T>(cap) -> (Sender<T>, Receiver<T>)`; disconnection happens when all senders (or
  receivers) are dropped. Workers therefore loop `match work.recv() { Ok(row) => …, Err(_) =>
  break }`; the main thread "closes" the work channel with `drop(work_s)` after the feed is done.
- **Reader: `hash_queue` ordering table in `db/hash.rs` (not a lazy cursor).**
  rusqlite statements hold a `&Connection` (and row iterators a `&Statement`), so storing an open
  prepared statement beside its owning `Connection` in one struct is forbidden by the borrow
  checker (`E0515`/`E0505`) — a long-lived lazy cursor *cannot be returned from `db/`* nor owned
  by archive code. The settled design (user-directed) encodes the work order in a scratch table:

  ```sql
  CREATE TABLE IF NOT EXISTS hash_queue (
      id      INTEGER PRIMARY KEY,              -- position 1..n, size-DESC order
      file_id INTEGER NOT NULL UNIQUE REFERENCES files(id)   -- UNIQUE => idempotent populate
  );
  ```

  - `create_hash_queue` — `CREATE TABLE IF NOT EXISTS` (idempotent).
  - `populate_hash_queue(conn, eager, hardlinks)` — idempotent (`INSERT OR IGNORE` via the
    `UNIQUE(file_id)`), uses **`count_all_hashable_files`'s WHERE** (phase + `ftype='file'` +
    hardlink-canonical + eager-filter; **no** `sha1`/error predicate) so the set is **stable
    regardless of hashing progress**; positions `row_number() OVER (ORDER BY size DESC, id)`. On
    resume it keeps existing rows and adds only missing ones, so the queue is invariant.
  - `pull_pending_hash_rows<R: SqlFileRow>(conn, index, limit) -> Result<Vec<(u64, R)>>` —
    `SELECT hash_queue.id AS pos, {R::sql_columns(Some("files"))} FROM files JOIN hash_queue
    ON hash_queue.file_id = files.id WHERE hash_queue.id > :index AND files.sha1 IS NULL
    AND (files.flags & :sha_error) = 0 ORDER BY hash_queue.id LIMIT :limit`, mapped via
    `R::from_row(r, Some("files"))`. The **pull keeps the pending filter** so a resume skips
    already-hashed rows; the queue only supplies the ordering. Returns `(position, record)` so
    the caller advances the index. O(n) on the queue PK + files PK join.
  - `drop_hash_queue` — `DROP TABLE IF EXISTS` on the **success** path only (never on
    interrupt/resume).

  Archive side (zero rusqlite): after `promote_unhasheable_files` →
  `create_hash_queue` + `populate_hash_queue`; the feed keeps a `queue_index: u64` (reset to 0
  each run — no persistence needed, the pull's `sha1 IS NULL` filter handles resume). Ordering is
  **`size DESC` restored** (biggest file first). The table lives in the work DB (a regular table,
  not SQLite `TEMP`), so it survives interrupts and resume.
- **`Database::with_transaction`** mirrors the existing `db/meta.rs::with_meta_txn` (`f(&tx)`
  where the closure param is `&Connection`); `pub(crate) mod hash;` was already present.
- **Workers receive `Shutdown` clones by value** (one `shutdown.clone()` per spawn) so the
  `move ||` closure moves only owned/copied values (`Builder::spawn` needs `F: Send + 'static`
  and `move` closures cannot borrow the outer loop `shutdown`/`page_size`); all other handles are
  fresh per-iteration clones/`let ps = page_size`. `hash_worker` therefore takes
  `shutdown: Shutdown` and passes `&shutdown` to `hash_one`/`check_between_files`.
- **Interrupted outcomes are discarded, not flagged.** The apply closure's `Err(e)` arm checks
  `e.err.is_interrupted()` and simply drops the outcome (row keeps `sha1 IS NULL`; resume
  re-picks it). This is a deliberate improvement over the broken committed rework, which marked
  interrupted files `ErrorWhileHash` — verified live: force-abort leaves 0 flagged rows.
- **`record_hash_error` signature fixed** to `e: &IdError` (the committed `&&IdError` was
  part of the non-compiling rework).
- **Thread-bar `mutex` re-entrancy bug fixed in `progress.rs::create_thread_bars`**: it used to
  lock `thread_bars` and then call `drop_thread_bars()` (same mutex) — a same-thread deadlock
  that hung the first hash run (main stuck in `futex_do_wait` before any worker spawned). The
  defensive drop now runs before taking the lock.
- Verification results on a 10k-file tree (+ two 76/57 MiB files) fit `--jobs 16`:
  - 16 per-worker byte bars stream the big files while small files finish concurrently (the
    target "no big-file barrier" behaviour), phase bar reaches n/n.
  - All 10002 stored digests equal `sha1sum` of the sources (0 mismatches).
  - Full `resume` run → archive written, exit 0.
  - Graceful SIGINT mid-hash: `hashing stopped; completed files saved saved=N`, pending rows
    intact, resume completes.
  - Force abort (3 spaced SIGINTs): `hashing force-aborted; in-flight progress discarded`,
    in-flight row not flagged, resume re-hashes and completes.
  - Piped run: stdout has zero ESC bytes, INFO/WARN on stdout, ERROR→stderr (unchanged).
  - Pre-existing unrelated failure noted: `extract` of the produced archive aborts in scan
    (`CHECK constraint failed: include_reason_extract >= 0`) — an extract-pipeline/schema
    inconsistency in the user's in-flight WIP scope, untouched by this change.