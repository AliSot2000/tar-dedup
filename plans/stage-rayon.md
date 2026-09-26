# Plan: Parallelize `stage` with rayon + batching, fail-fast on structural errors

Status: **implemented (2026-09-26)** — decisions settled with the user, implementation complete and verified.
Replace the sequential stage loop with a batched rayon worker pool over
`archive/stage.rs`, with an explicit **fail-fast** policy for structural errors
(permissions / disk-full), cursor-based batched listing in `db/stage.rs`, and
progress totals. No new per-file flags (`ErrorWhileStaging` / `HasStage`).

## Motivation

Symlink creation is O(1) regardless of file size — the stage phase is pure
syscall I/O and parallelizes trivially. The only failure modes that matter are
structural (stage-dir permissions, disk full, read-only FS): nothing in the
program can repair them, so the phase must abort and hand control back to the
caller for cleanup — **not** record-and-continue into an archive that is
missing (almost) everything.

## Decisions (user-confirmed)

1. **Unconditional abort** on structural errors — do not gate on `--fail-fast`.
2. **Batching + rayon together now** — 10_000-row batches with an id cursor,
   plus a rayon pool inside each batch (bounds wasted syscalls on a full disk
   and avoids an unbounded `Vec` pull).
3. **Worker count = `config.process.io_jobs`** (I/O-bound pool, matching
   `place.rs` / `sparsify` / `dedup`).

## Current state (verified)

- `archive/stage.rs` (94 lines) — sequential `for record in file_vec` over
  `db.list_files_to_stage()` (single unbounded query, no cursor). Per record:
  resolve source (sparse file via `HasSparse`→`sparse_member_name`, else
  `abs_path`), `warn_if_times_changed`, remove any stale target, then
  `symlink(source, target)`. On any symlink/remove failure:
  `record_error(...); return Err(Error::io(&target, e))`. So it already *aborts*
  on error — but serially, with no batching, no progress totals, and `// TODO`
  comments for progressbar / batching / fail-fast / stage flags.
- `db/stage.rs` — `promote_unstageable_files` (phase→`staged` for ineligible:
  non-file, non-self-canonical, filter-fail, null sha) and
  `list_files_to_stage<R>(conn)` (unbounded `SELECT` on `sparsified` files).
- `tar_builder.rs` consumes `phase='staged'` self-canonical rows
  (`list_staged_canonical_ordered`). If a staged symlink is missing it records
  `ErrorWhileArchive` and either `fail_fast`-aborts or skips.
- `archive.rs` phase loop: `Err(Error::Interrupted)` → save state, exit clean
  (resumable); any other `Err(e)` → `return Err(e)` to the caller. So stage
  returning a non-Interrupted error preserves the work dir for a resumed run
  and never writes an empty archive.
- Precedent for rayon + fail-fast + `Mutex<Vec<Result>>` drain:
  `unarchive/place.rs` (`copy_canonicals_to_source`, `link_into_place`,
  `build_file_tree`, …) and `unarchive/rehash.rs`:
  `ThreadPoolBuilder::new().num_threads(config.process.io_jobs)`,
  `par_iter().try_for_each(|..| -> Result<()>)`, push into a
  `results: Mutex<Vec<…>>`, drain on the main thread, then
  `match parallel { Ok(()) / Err(Error::Interrupted) / Err(e) }`.
- **DB single-threaded**: workers must not touch `Database` (rusqlite `Connection`
  is a per-process handle; progress/DB apply stay on the main thread), matching
  place/rehash exactly.
- Stage bar kind: `BarKind::Counter` (`archive.rs` `enter_phase`); progress
  totals/position are currently unset in `stage::run`.
- Flag design rationale: `HasSparse` is a *semantic source selector* consumed by
  stage/tar; `phase='staged'` already records staging success. The
  `ErrorWhile*` flags exist for per-file *recoverable* faults. Stage has no such
  per-file fault (even a missing source just yields a dangling symlink, caught
  later at tar `canonicalize`), so adding `ErrorWhileStaging`/`HasStage` would be
  redundant or actively harmful (invites record-and-continue → empty archive).

## Design

### `db/stage.rs`

Add a cursor-based listing (random-access is safe because payload target names
are content-unique):

```sql
SELECT {R::sql_columns(None)} FROM files
WHERE phase = 'sparsified'
  AND ftype = 'file'
  AND canonical_id = id
  AND {generate_archive_filter(None)}
  AND sha1 IS NOT NULL
  AND id > :last
ORDER BY id
LIMIT :limit
```

`pub fn list_files_to_stage_after<R: SqlFileRow>(
    conn: &Connection, last_id: u64, limit: u64) -> Result<Vec<R>>`

Keep the existing `list_files_to_stage` (still referenced) or fold callers onto
the new one — prefer keeping both to minimize churn; the old one can delegate
(`list_files_to_stage_after(conn, 0, u64::MAX)`).

### `db.rs`

Add facade:

```rust
pub fn list_files_to_stage_after<R: SqlFileRow>(
    &self, last_id: u64, limit: u64) -> Result<Vec<R>>
```

### `archive/stage.rs`

Restructure `run` into a batched rayon loop:

```rust
const BATCH_SIZE: u64 = 10_000;
const ERROR_PHASE: ErrorPhase = ErrorPhase::Pipeline(PipelinePhase::Stage);

pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    let config = rt.config;
    let db = rt.db;
    let shutdown = rt.shutdown;

    fs::create_dir_all(config.paths.stage_dir())
        .map_err(|e| Error::io(&config.paths.stage_dir(), e))?;

    let promoted = db.promote_unstageable_files()?;
    rt.progress.inc_global(promoted);
    tracing::info!(promoted, "promoted n/a → staged which aren't eligible");

    let total = db.count_files_in_phase(FilePhase::Sparsified)?; // or a dedicated count
    let already = db.count_files_in_phase(FilePhase::Staged)?;   // resume anchor
    rt.progress.set_phase_total(total);
    rt.progress.set_phase_position(already);

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(config.process.io_jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;
    let mut recorder = Recorder::new(db, !config.process.no_errors);
    let mut last_id = 0u64;
    let mut staged = 0u64;

    loop {
        shutdown.check_between_files()?;
        let batch: Vec<StrippedRecord> =
            db.list_files_to_stage_after::<StrippedRecord>(last_id, BATCH_SIZE)?;
        if batch.is_empty() { break }
        last_id = batch.last().expect("batch non-empty").id.0;

        let n = batch.len() as u64;
        let results: Mutex<Vec<Result<FileId, (FileId, Error)>>> = Mutex::new(Vec::new());

        let parallel = pool.install(|| {
            batch.par_iter().try_for_each(|record| -> Result<()> {
                let source = if record.flags.get(FileFlag::HasSparse) {
                    let sparse_name = record.sparse_member_name()
                        .expect("stage: Expected only canonical sparse files");
                    config.paths.stage_dir().join(sparse_name).clean()
                } else {
                    record.abs_path.to_path_buf()
                };
                let tar_name = record.tar_member_name()
                    .expect("stage: Expected only canonical files");
                warn_if_times_changed(&source, record.mtime, record.atime, record.ctime);
                let target = config.paths.stage_dir().join(tar_name);
                if target.exists() {
                    match fs::remove_file(&target) {
                        Ok(()) => (),
                        Err(e) => {
                            results.lock().expect("stage results lock").push(
                                Err((record.id, Error::io(&target, e))));
                            return Err(Error::io(&target, e));
                        }
                    }
                }
                match symlink(&source, &target) {
                    Ok(()) => {
                        results.lock().expect("stage results lock")
                            .push(Ok(record.id));
                        Ok(())
                    }
                    Err(e) => {
                        results.lock().expect("stage results lock").push(
                            Err((record.id, Error::io(&target, e))));
                        Err(Error::io(&target, e))   // short-circuit this batch
                    }
                }
            })
        });

        // Main-thread apply (never in workers).
        for outcome in take(&mut *results.lock().expect("stage results lock")) {
            match outcome {
                Ok(id) => {
                    db.mark_file_phase(id, FilePhase::Staged)?;
                    staged += 1;
                }
                Err((id, Error::FileStat(fse))) => {
                    recorder.record_file(id, ERROR_PHASE, fse, ErrorFlags::default());
                }
                Err((_id, other)) => panic!(
                    "INVARIANT FAILED: stage worker may only return FileStat/Interrupted. Got: {other}"
                ),
            }
        }
        rt.progress.inc_both(n);

        match parallel {
            Ok(()) => (),
            Err(Error::Interrupted) => {
                recorder.flush()?;
                return Err(Error::Interrupted);
            }
            Err(e) => {
                recorder.flush()?;
                return Err(e);   // FAIL-FAST to caller
            }
        }
    }

    recorder.flush()?;
    tracing::info!(staged, "stage complete");
    Ok(())
}
```

Notes:
- `take` / `std::mem::replace`-style drain mirrors place.rs (use
  `std::mem::take` on the `Mutex` wrapped Vec).
- Interrupt handling: `parallel` returns `Err(Error::Interrupted)` when a worker
  hits `check_between_files` / force; in-flight workers finish their current
  record and push outcomes, which the drain applies before returning
  `Interrupted` → the archive loop saves state; a resumed run only stages the
  remaining `sparsified` rows (already-staged fall out of the predicate).
- The failing record is recorded in the persistent error log (diagnosis) *and*
  the phase still aborts — no record-and-continue.
- Progress totals: prefer a `count_files_in_phase(Sparsified)` total mirroring
  other phases; the already-staged count is the resume position. (Verify
  `Database::count_files_in_phase` exists — it does, db.rs:145.)

## Files touched

1. `crates/tar-dedup/src/db/stage.rs` — `list_files_to_stage_after` (+ delegate
   old `list_files_to_stage` if kept).
2. `crates/tar-dedup/src/db.rs` — facade.
3. `crates/tar-dedup/src/archive/stage.rs` — rayon batch loop, fail-fast,
   progress totals.
4. `Readme.md` (optional) — one line: stage is parallel and aborts on stage
   integrity errors.

## Tests

- Small unit test for `list_files_to_stage_after` cursor slicing (mirror
  `list_pending_comparisons_slices_and_orders`): seed N eligible `sparsified`
  rows, walk with `last_id`/limit, assert full coverage + no overlap.
- Keep the existing stage behaviour tests green (no stage-specific test module
  exists today; the phase is exercised by full-archive smokes).

## Verification

- `cargo build -p tar-dedup -p tar-dedup-cli`
- `cargo test -p tar-dedup --lib` — expect the 4 pre-existing failures
  (inventory ×2, scan ×2) + 1 ignored only.
- Manual smoke:
  - normal archive run still writes an archive with all stage symlinks;
  - chmod-000 the stage dir → `stage` aborts with an `Err` (no archive written),
    work dir intact;
  - `chmod` back, `resume`/rerun → staging completes.
- No `--release`; no `cargo fmt`.

## Notes / decision log

- Structural-failure set includes EACCES/EPERM, ENOSPC/EDQUOT, EROFS, EIO —
  everything `symlink`/`remove_file` can realistically hit on the stage dir.
- Missing *source* does not fail `symlink` (dangling symlink); it surfaces later
  at tar `canonicalize` → `ErrorWhileArchive` (pre-existing, out of scope).
- No new flags: `phase='staged'` is the success marker;
  `ErrorWhileStaging` would only enable the empty-archive failure mode.
- Batching cursor is `id > :last ORDER BY id`; stable across the 10_000 window
  because IDs never change mid-phase.