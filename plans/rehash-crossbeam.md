# Plan: rehash phase → crossbeam producer/consumer (mirror archive/hash.rs)

Status: **implemented 2026-10-01** (build green — `cargo build`/`cargo build -p tar-dedup`; manual smoke from §Verification still to run).

## Context / why

The extract `rehash` phase currently uses a rayon pool:

```rust
let pending = rt.db.files_in_phase(FilePhase::Unarchived)?; // TODO that's wrong
let pool = ThreadPoolBuilder::new().num_threads(jobs)...;
let results = Mutex::new(Vec::<RehashOutcome>::new());
let parallel = pool.install(|| pending.par_iter().try_for_each(...));
```

Problems:

1. **Selection is wrong / stale.** It pulls every row in phase `unarchived`, but the extract
   `Filter` phase now ends with `global_mark_phase(FilePhase::ExtractFiltered)` (commits
   `866c867` + `a5e8ab5`), so by the time `Rehash` runs **every** row is `extract_filtered` —
   `files_in_phase(Unarchived)` would select nothing, and `skip_rehash` (`WHERE phase =
   'unarchived'`) would promote nothing. The correct target is **`extract_filtered`**.
2. **Criteria is ad-hoc.** Rehashed payloads are only those the scan actually extracted and
   verified: the row must be a **self-canonical regular file** (`canonical_id = id`) that
   carries **`FileExtracted`** and passes the **joint archive+extract filter** — plus it must
   have a stored `sha1` to compare against. The existing `db/rehash.rs::list_files_to_rehash`
   already encodes exactly this predicate (minus `sha1 IS NOT NULL`), but it is **unused** and
   has no ordering.
3. **No ordering / resume-consistent progress.** Nothing encodes a work order, and there are no
   progress counts. `run` just sets `set_phase_total(pending.len())` from the wrong list.
4. **Big-file barrier + unbounded payload list.** The whole `unarchived` list is materialized in
   RAM and every worker outcome is held in a `Mutex<Vec>` until the entire batch finishes — same
   two issues `archive/hash.rs` fixed by moving to a bounded crossbeam pipeline with an ordering
   table.

**Decision (user-approved):** rebuild `unarchive/rehash.rs` on the exact crossbeam
producer/consumer pattern of `archive/hash.rs` (bounded work/out channels, `effective_jobs()`
owned `std::thread` workers, caller-side feed/drain loop, per-thread byte bars, interrupt
policy), and mirror the DB-side additions of `db/hash.rs` (`rehash_queue` ordering table,
batched outcome ingest, promote-non-elected step).

## Settled questions (from review)

| # | Question | Decision |
|---|----------|----------|
| 1 | Which phase do count/populate/pull select from? | **`extract_filtered`** everywhere (and `skip_rehash` retargeted `'extract_filtered' → 'rehashed'`). |
| 2 | What happens to non-elected rows (dupes, filter-excluded, non-file, sha-less)? | **Bulk-promote at the start of the phase** via `promote_unrehashable_files` (`extract_filtered → rehashed`), mirror of hash's `promote_unhasheable_files`. |
| 3 | Counting functions? | **Only `count` (total in scope) + `count_done` (already `rehashed`)**. Pending is derived: `pending = count − count_done`. No `count_pending`/`count_todo` function. |
| 4 | Outcome type + batch ingest? | **Move `RehashOutcome` into `db/rehash.rs`** and add a transactional `ingest_rehash_outcome(&[RehashOutcome])`, matching how `db/hash.rs` owns `HashingOutcome`/`ingest_hash_outcome` (and `db/dedup.rs` owns `CompareOutcome`/`ingest_compare_outcome`). |
| 5 | Per-thread bars? | **Yes** — `BarKind::Bytes` thread bars like hash. Phase bar stays `BarKind::Count`. |

## Non-goals / do NOT do

- **No new unit tests.** Test suite remains a deferred follow-up (AGENTS.md: "tests are a
  work in progress — do not block on them"). Do not add a `mod tests` block to rehash files; fix
  any pre-existing failures in these files on their own merits only if they block the build.
- No schema addition to `db/schema.rs` — `rehash_queue` is created at runtime by
  `create_rehash_queue` (same approach as `hash_queue`).
- Do NOT touch `archive/hash.rs`, `db/hash.rs` (except as read-only reference), `unarchive.rs`
  phase order, `db/common.rs` filter generators, or the scan phase.
- Do NOT alter the extract `Filter` phase / `global_mark_phase(ExtractFiltered)` behavior.
- Keep interrupt semantics identical to today (graceful = finish in-flight file + save completed;
  force = discard in-flight). Preserve the mismatch/error endgame including `--force` override
  and the persistent error log (`Recorder`).

## Repo state / starting point

- Working tree has **uncommitted** WIP in `src/db/extract.rs`, `src/db/scan.rs`,
  `src/unarchive/rehash.rs`, `src/unarchive/scan.rs` (user's in-flight scan/filter phase
  rework) plus untracked `docs/files-flow-{archive,extract}.{png,svg}`. Do not touch the scan
  WIP; the rehash rewrite builds on the post-filter `extract_filtered` world it establishes.
- `unarchive/rehash.rs` `stat_and_apply_outcomes` currently applies outcomes one-by-one; it will
  be replaced by `db::ingest_rehash_outcome` + a caller-side `Recorder`. The diff currently in
  the file is a formatting-only change to `stat_and_apply_outcomes` — the rewrite supersedes it.
- Reference implementation to mirror structurally: `archive/hash.rs` (constants, worker shape,
  feed/drain loop, `at_least_one_running`, interrupt + message wording) and its DB mirror
  `db/hash.rs` (`create/populate/pull/drop_hash_queue`, `count_all/pending_hashable_files`).

## DB changes — `crates/tar-dedup/src/db/rehash.rs`

### Shared WHERE fragment

```rust
/// Self-canonical, FileExtracted, joint-filtered, regular files with a digest.
fn files_to_rehash_where() -> String
// canonical_id = id
//   AND (flags & :extracted) != 0
//   AND {generate_archive_and_extract_filter(None)}
//   AND ftype = 'file'
//   AND sha1 IS NOT NULL
```

(`:extracted` = `FileFlag::FileExtracted.mask_i64()`; `sha1 IS NOT NULL` is the "TODO also
filtr for sha" from the old `run`.)

### Counts (progress only — pending is derived)

```rust
pub fn count_files_to_rehash(conn: &Connection) -> Result<u64>   // COUNT over files_to_rehash_where()
pub fn count_rehashed_files(conn: &Connection) -> Result<u64>    // files_to_rehash_where() AND phase = 'rehashed'
```

- `count` = stable set **regardless of phase** (same rows `populate_rehash_queue` orders), so the
  bar length is invariant across resumes.
- `count_done` = rows already `rehashed` (accepted on a previous run) → resume position.
- `pending` = `count − count_done`, used for "nothing to do" short-circuit and the final
  `completed == pending` assertion.

### Bulk promote (start of phase)

```rust
pub fn promote_unrehashable_files(conn: &Connection) -> Result<u64>
// UPDATE files SET phase = 'rehashed'
// WHERE phase = 'extract_filtered'
//   AND NOT (canonical_id = id AND ftype = 'file' AND sha1 IS NOT NULL
//            AND (flags & :extracted) != 0 AND {joint filter})
```

Covers dupes (`canonical_id != id` / `canonical_id IS NULL`), directories/symlinks/etc.
(`ftype != 'file'`), extract-filter-excluded rows, and rows lacking a digest. Mirrors
`db/hash.rs::promote_unhasheable_files`. Runs **before** the queue is populated (so the queue
only ever sees elected rows).

### Ordering table (mirror of `hash_queue`)

```rust
pub fn create_rehash_queue(conn: &Connection) -> Result<()>
// CREATE TABLE IF NOT EXISTS rehash_queue (
//     id      INTEGER PRIMARY KEY,
//     file_id INTEGER NOT NULL UNIQUE REFERENCES files(id)
// )

pub fn populate_rehash_queue(conn: &Connection) -> Result<u64>
// INSERT OR IGNORE INTO rehash_queue (id, file_id)
// SELECT row_number() OVER (ORDER BY size DESC, id), id
// FROM files WHERE {files_to_rehash_where()}
// → idempotent across resume (UNIQUE(file_id) + INSERT OR IGNORE)

pub fn pull_pending_rehash_rows<R: SqlFileRow>(conn: &Connection, index: u64, limit: u64)
    -> Result<Vec<(u64, R)>>
// SELECT rehash_queue.id AS pos, {R::sql_columns(Some("files"))}
// FROM files JOIN rehash_queue ON rehash_queue.file_id = files.id
// WHERE rehash_queue.id > :index
//   AND files.phase = 'extract_filtered'      -- the pending predicate
// ORDER BY rehash_queue.id
// LIMIT :limit
// mapped via |r| { pos = r.get("pos"); rec = R::from_row(r, Some("files")); }

pub fn drop_rehash_queue(conn: &Connection) -> Result<()> // DROP TABLE IF EXISTS — success path only
```

- Pull keeps the **phase** predicate so a resume skips already-`rehashed` rows; the queue only
  supplies the size-DESC order. (hash filters on `sha1 IS NULL`; rehash's "done" marker is the
  phase itself.)
- Do NOT add `(flags & ErrorWhileRehashing) = 0` to the pull: hash rebuilds digest + flag on
  retry; rehash retries the *file read* but must still re-verify on resume
  (`ingest_rehash_outcome` only sets the flag alongside `phase='rehashed'`). Keep the predicate
  minimal (phase only) unless a retry-loop is observed — note this in the merge message.

### Outcome type + batched ingest (moved into `db/`)

```rust
pub enum RehashOutcome {
    Match(FileId),      // digest matches catalog sha1 (or no payload to verify — dup row)
    Mismatch(FileId),   // digest differs from catalog sha1
    Error(FileId),      // IO / missing cache / missing expected digest
    Failed(FileId, String, Option<PathBuf>), // rehash failed; carried message + cache path
}

pub fn ingest_rehash_outcome(conn: &mut Connection, results: &Vec<RehashOutcome>) -> Result<u64>
// single transaction (like db/hash.rs::ingest_hash_outcome):
//   Match    → mark_file_phase(id, Rehashed)
//   Mismatch → set_file_flag(id, RehashMismatch, true) + mark_file_phase(id, Rehashed)
//   Error    → set_file_flag(id, ErrorWhileRehashing, true) + mark_file_phase(id, Rehashed)
//   Failed   → set_file_flag(id, ErrorWhileRehashing, true) + mark_file_phase(id, Rehashed)
```

`RehashOutcome` moves **out of** `unarchive/rehash.rs`. Persistent error-log rows for
`Error`/`Failed` stay on the caller side (a `Recorder`, exactly like
`archive/hash.rs::record_hash_error`) — the DB fn does flags+phase only.

### `skip_rehash` retarget

```rust
// UPDATE files SET phase = 'rehashed' WHERE phase = 'extract_filtered'
```
(move this WHERE from `'unarchived'`). The DB-less/`!config.scan.rehash` path then promotes the
same rows the new pending query would have.

### Remove

`list_files_to_rehash` (superseded by the queue pull; currently unused by the extract pipeline).

## Database facade — `crates/tar-dedup/src/db.rs`

Add thin delegating methods (all in `mod rehash`):

```rust
pub fn count_files_to_rehash(&self) -> Result<u64>
pub fn count_rehashed_files(&self) -> Result<u64>
pub fn promote_unrehashable_files(&self) -> Result<u64>
pub fn create_rehash_queue(&self) -> Result<()>
pub fn populate_rehash_queue(&self) -> Result<u64>
pub fn pull_pending_rehash_rows<R: SqlFileRow>(&self, index: u64, limit: u64) -> Result<Vec<(u64, R)>>
pub fn drop_rehash_queue(&self) -> Result<()>
pub fn ingest_rehash_outcome(&self, results: &Vec<RehashOutcome>) -> Result<u64>
```

Update the `skip_rehash` delegation doc to note the `extract_filtered` target. Remove the
`list_files_to_rehash` facade.

## Pipeline rewrite — `crates/tar-dedup/src/unarchive/rehash.rs`

### Constants

```rust
const BATCH_SIZE: u64 = 10_000;              // legacy knob kept in step with hash's BATCH_SIZE
const WORK_CAPACITY = BATCH_SIZE as usize;   // input queue bound = memory guard
const OUT_CAPACITY  = 2 * WORK_CAPACITY;
const FEED_CHUNK    = 1_024;                 // rows pulled per feed round
const DRAIN_CHUNK   = BATCH_SIZE as usize / 2; // outcomes committed per transaction
```

### `run(rt)` shape

1. **Counts + promote** (mirror hash's opening):
   ```rust
   let total = db.count_files_to_rehash()?;
   let done  = db.count_rehashed_files()?;
   tracing::info!(files = total, jobs = …, "rehash pass");  // keep existing wording
   if !rt.config.scan.rehash {
       let n = db.skip_rehash()?;
       rt.progress.inc_global(n);
       tracing::info!(promoted = n, "rehash skipped; extract_filtered → rehashed");
       return Ok(());
   }
   let pending = total.saturating_sub(done);
   if pending == 0 { return Ok(()); }
   let promoted = db.promote_unrehashable_files()?;
   tracing::info!("Promoted {promoted} entries which cannot be rehashed.");
   rt.progress.inc_global(total - done - pending); // ≈ 0 only if count==done+... ; promote accounting
   // NOTE: promote runs before set_phase_total below is *not* a problem — count/count_done were
   // captured before the promote, so bar length = the elected set, matched to populate.
   rt.progress.set_phase_total(total);
   rt.progress.set_phase_position(done);
   ```
   (The `inc_global(promoted)` bytes: mirror hash's `inc_global(total_entries - hash_needed)` —
   rows leaving the phase by SQL, not through the bars. The promoted rows are *not* part of the
   phase bar's `total`; they are accounted on the global bar only. Adjust the exact arithmetic at
   implementation time so global = table-progress stays monotone: `inc_global(promoted)`.)

2. **Queue**:
   ```rust
   db.create_rehash_queue()?;
   db.populate_rehash_queue()?;
   ```

3. **Thread bars**: `progress.create_thread_bars(BarKind::Bytes, jobs)`; materialize one
   `ProgressBar` per worker into a `Vec` (by value) before spawning.

4. **Channels**: `bounded::<StrippedRecord>(WORK_CAPACITY)` and
   `bounded::<Option<RehashOutcome>>(OUT_CAPACITY)`.

5. **Workers**: `thread::Builder::name("rehash-worker-{i}")` × `effective_jobs()`, each `move ||`
   capturing its cloned bar + `Shutdown` clone + cloned `Receiver`/`Sender` handles:

   ```rust
   fn rehash_worker(bar: ProgressBar, shutdown: Shutdown, work: Receiver<StrippedRecord>,
                    out: Sender<Option<RehashOutcome>>) {
       loop {
           match work.recv() {
               Ok(row) => {
                   if shutdown.check_between_files().is_err() { break; }
                   bar.reset();
                   bar.set_length(row.size);
                   bar.set_message(format!("Rehashing {}", row.abs_path.display()));
                   let outcome = rehash_one(&stage_dir, &row, &shutdown, Some(&bar));
                   out.send(Some(outcome)).expect("rehash worker: result channel closed");
               }
               Err(_) => break
           }
       }
       out.send(None).expect("rehash worker: result channel closed");
   }
   ```
   - `rehash_one` gains a `pb: Option<&ProgressBar>` param and `hash_file` advances it per read
     (`pb.inc(n as u64)`), mirroring `hash_one`'s live byte bar. Keep `hash_file` SHA-1 only (no
     zero-page counting — rehash verifies digests, sparseness was already settled at catalog time).
   - Workers never touch `ProgressBarSet` or the DB.

6. **Main feed/drain loop** (inline; model on `handle_send_receive_loop` but keep it a private
   helper `handle_send_receive_loop(rt, work_s, out_r, one_running) -> Result<u64>` so `run`
   stays readable):
   - `recorder = Recorder::new(rt.db, !rt.config.process.no_errors)` at top.
   - `queue_index: u64` (starts 0; reset each run — the pull's `phase='extract_filtered'`
     predicate alone handles resume), `feed_buf`, `feed_idx`, `feed_exhausted`, `busy`.
   - `apply_chunk(&mut pending_out)`: `db.ingest_rehash_outcome(&items)?`; then for each
     `Error`/`Failed` outcome push a `FileStatError::General { path, message }` (Failed's cached
     path; Error → `path: None, message: "rehash failed for file {id}"`) through the recorder
     (same `ErrorPhase::Extract(ExtractPipelinePhase::Rehash)` as today); `progress.inc_both(n)`.
   - `drain_chunk`: `try_recv` up to `DRAIN_CHUNK`; appends `Option<RehashOutcome>`; apply when
     full or on final drain. `Ok(None)` (worker exit marker) increments the exited counter.
   - Feed loop: `try_send` buffered `(pos, record)` pairs; `is_interrupted()` breaks the feed.
   - Termination (copy hash's reasoning): drop `work_s`; drain loop until `dequeue_total ==
     feed_total` OR `!one_running()` OR exited == jobs OR interrupted; final `apply_chunk`
     override; `drop(out_r)`; `recorder.flush()?`; return `completed`.

7. **Endgame** — keep today's contract:
   ```rust
   let force = shutdown.is_force();
   match handle_send_receive_loop(..) {
       Ok(completed) => {
           progress.drop_thread_bars();
           match shutdown.is_interrupted() {
               true => { warn(saved/completed, "rehash stopped; completed files saved"
                                  | "rehash force-aborted; in-flight progress discarded");
                         Err(Error::Interrupted) }
               false => {
                   db.drop_rehash_queue()?;
                   let mismatches = db.count with RehashMismatch flag … (or carry a counter out)
                   if mismatches > 0 && !rt.config.force { return Err(Error::Config("Corruption
                       detected: … --force")) }
                   warn(errors…) ;
                   tracing::info!(matches, mismatches, errors, "rehash complete");
                   Ok(())
               }
           }
       }
       Err(e) => { progress.drop_thread_bars(); Err(e) }
   }
   ```
   - Counters: have `ingest_rehash_outcome` return applied row count and let the loop maintain
     `matches/mismatches/errors` by scanning the applied chunk (or return the triple from a small
     caller-side tally — the `RehashOutcome` variants map 1:1). Keep the mismatch-message wording
     ("Ignore this error with --force") byte-identical.

## File-by-file change list

| File | Change |
|---|---|
| `crates/tar-dedup/src/db/rehash.rs` | add `files_to_rehash_where`, `count_files_to_rehash`, `count_rehashed_files`, `promote_unrehashable_files`, `create_rehash_queue`, `populate_rehash_queue`, `pull_pending_rehash_rows`, `drop_rehash_queue`, `RehashOutcome`, `ingest_rehash_outcome`; retarget `skip_rehash` to `'extract_filtered'`; remove `list_files_to_rehash`. |
| `crates/tar-dedup/src/db.rs` | facades for the above (replacing `list_files_to_rehash`). |
| `crates/tar-dedup/src/unarchive/rehash.rs` | full pipeline rewrite: crossbeam workers + feed/drain loop + thread bars; `rehash_one`/`hash_file` gain a `pb` param; delete rayon pool + `stats_and_apply_outcomes` + local `RehashOutcome`; caller-side recorder for Error/Failed. |

## Verification (no new tests — build + manual smoke)

1. `cargo build -p tar-dedup` (baseline suite: 245 passed / 4 pre-existing fails / 1 ignored —
   those 4 and the scan WIP are out of scope; only fix if the rewrite itself breaks the build).
2. Manual smoke: archive a small multi-file tree (with dupes + a dir + a symlink + one
   filter-excluded path), extract with rehash enabled — confirm:
   - INFO line "rehash pass" shows `files == elected count` (not the whole table);
   - `elected` rows go `extract_filtered → rehashed`, dupes/non-files/filter-excluded are
     promoted by the bulk step;
   - phase bar length = `count`, starts at `count_done` on a resumed run, per-thread byte bars
     move during verification;
   - graceful `SIGINT` mid-rehash → "rehash stopped; completed files saved"; resume completes and
     `drop_rehash_queue` only on success (queue table survives an interrupt);
   - force abort → "rehash force-aborted; in-flight progress discarded"; in-flight row still
     `extract_filtered` after resume re-verifies it.
3. Corrupt one cached payload (`dd`/truncate a file in the `.estage` cache) → digest mismatch
   path: `RehashMismatch` flag + `--force` override behavior unchanged; without `--force` →
   `Error::Config("Corruption detected …")`.

## Follow-ups (recorded, NOT this round)

- Test suite for the rehash phase (deferred per AGENTS.md).
- The `stage_dir` vs `extract_cache_dir` naming on the extract side (`stage_dir()` and
  `extract_cache_dir()` currently both return `work_dir`); reuse of an extracted-payload constant.
- Whether `pull_pending_rehash_rows` should also exclude `(flags & ErrorWhileRehashing) != 0`
  rows from auto-retry (today the flag is set *alongside* `phase='rehashed'`, so the phase
  predicate is already sufficient — revisit only if a retry loop is observed).