# Plan: Place stage crossbeam + size-DESC + inner-loop progress

Status: **implemented** (2026-10-03). Build green, 272/4 lib tests (same 4
pre-existing failures). **No tests in this round** (a place test battery is deferred,
matching how hash/rehash were sequenced). `materialize_link_tree` and
`prepare_extraction_dir` are untouched.

## As-built

- `db/place.rs`: `canonical_move_queue(id, file_id UNIQUE REFERENCES files)`,
  `materialize_queue(id, out_tree_id UNIQUE REFERENCES out_tree)` with create /
  populate (`row_number() OVER (ORDER BY size DESC, id)`) / pull (queue position
  + pending re-filter) / drop (success-path only).
  `pull_canonical_move_queue` re-filters `flags & AtLinkSource = 0`;
  `pull_materialize_queue` re-filters `Placed = 0 AND ErrorWhilePlace = 0` and reads
  the dedup canonical via `f.canonical_id = c.id` (returns `(pos, canonical, out)`).
  All rusqlite in `db/`, facades in `Database`.
- `count_files_to_move` / `count_moved_files` mirror `count_files_to_rehash` /
  `count_rehashed_files`: the **overall** move workload (stable set, no
  `AtLinkSource` predicate) and, of it, the already-moved count. The moved
  sub-bar sets `length = total` and `position = done`; `pending = total − done`
  drives the early-return. (A first cut used the pending remainder as the bar
  max, which made resumed runs under-report.)
- `copy_canonicals_to_source`: crossbeam worker pipeline over `send_receive_loop`
  (W=`StrippedRecord`, O=`Result<(FileId,bool),(FileId,Error)>`); per-worker Bytes
  thread bar; separate `files moved` Count sub-bar (`push_sub_bar`); **no
  phase/global increments** (owned by `materialize_link_tree`). Cache-source removal
  after a successful copy when `!keep_stage` (confirmed inverted-`remove_file(dst)`
  fix). Worker `check_between_files` between rows; force-abort drops the in-flight
  outcome (`None`); queue dropped / `Err(Interrupted)` handled like hash/rehash.
- `materialize_files` / `materialize_hardlinks` / `materialize_others` are three thin
  wrappers over the private `materialize_loop(rt, recorder, kind)` with
  `enum MaterializeKind { Files, Hardlinks, Others }` and a common
  `MaterializeWork { canonical: FileRecord, out: OutTreeRecord, out_canon: Option<OutTreeRecord> }`
  (`#[derive(Clone)]`; the shared loop requires `W: Clone`). `Files` is size-DESC via
  `materialize_queue`; hardlinks/others keep the `id`-cursor listers
  (now already-`FileRecord`, skipping the old `to_stripped()` downgrade at the lister).
  Workers reuse one template; single-action kinds get `ProgressBar::hidden()` bars
  (no noise), copy kind gets Bytes thread bars. `process_results` (ingest + recorder)
  reused verbatim; phase/global advance `inc_both(1)` per item in the apply closure.
- `copy_single_file` gained `pb: Option<&ProgressBar>`; the sparse-copy
  `on_progress` callback advances the bar by read deltas (and still runs
  `check_in_flight`); reflink path untouched.
- Worker contract mirrors hash/rehash: `check_between_files`, trailing `None`,
  force mid-copy → `None` (discard in-flight, row stays pending), graceful finishes
  the in-flight entry. `recorder.flush()` once after each loop; auto-flush (10k)
  otherwise. `io_jobs` = worker count everywhere.

## Goal

Adopt the shared `common::send_receive_loop` for the extract placement stage:

- `copy_canonicals_to_source` (link-tree mode) — crossbeam worker pipeline, size-DESC
  ordering, its own progress bars, **no phase/global increments**.
- `materialize_files` / `materialize_hardlinks` / `materialize_others` (non-link mode) —
  merged into one `materialize_loop(rt, recorder, kind)` with a `MaterializeKind` enum;
  `materialize_files` gets size-DESC ordering.
- Replace the outer `rt.progress.inc_both(n)` batch updates with **inner-loop progress**
  from the worker threads + per-worker thread bars.

## Copy semantics fix (user-confirmed)

`unarchive/place.rs` currently does, after copying `cache/{cid}` → `.sources/{cid}`:

```rust
if !rt.config.process.cleanup.keep_stage {
    let _ = fs::remove_file(dst);   // ← deletes the just-written link source
}
```

That is inverted. The `.sources` copy is the thing `materialize_link_tree` links FROM and
must survive. Confirmed intent: remove **the extract-cache source** of files that were
copied **successfully**, when `!keep_stage`:

```rust
Ok((id, _)) if !keep_stage => { let _ = fs::remove_file(&src); ... }
```

Failed / interrupted copies leave the cache file intact. (The whole cache dir is already
`remove_dir_all`-ed at the end of `place::run` when `!keep_stage`, so this is just
early cleanup.)

## 1. Size-DESC ordering — two queue tables (hash/sparsify pattern)

A monotonic `last_id` cursor cannot page by size, and `send_receive_loop` refills by
advancing a cursor, so mirror `hash_queue` / `rehash_queue`:

### `canonical_move_queue` (→ `copy_canonicals_to_source`)
- DDL: `CREATE TABLE IF NOT EXISTS canonical_move_queue (id INTEGER PRIMARY KEY,
  file_id INTEGER NOT NULL UNIQUE REFERENCES files(id))` — idempotent.
- Populate (idempotent, `INSERT OR IGNORE`):
  `SELECT row_number() OVER (ORDER BY f.size DESC, f.id), f.id FROM files f
   WHERE f.flags & :extracted != 0 AND f.flags & :moved = 0 AND f.ftype = 'file'
     AND f.phase = 'rehashed' AND {joint archive+extract filter}` (`:moved` =
  `FileFlag::AtLinkSource`).
- Pull by queue position + `flags & AtLinkSource = 0` re-filter (so refilling the feed
  buffer never re-returns a handed or done row — same contract as `hash_queue`).
- Drop on success only (mirror `drop_hash_queue`).

### `materialize_queue` (→ `materialize_files`)
- DDL: `CREATE TABLE IF NOT EXISTS materialize_queue (id INTEGER PRIMARY KEY,
  out_tree_id INTEGER NOT NULL UNIQUE REFERENCES out_tree(id))`.
- Populate: `SELECT row_number() OVER (ORDER BY f.size DESC, o.id), o.id
  FROM out_tree o JOIN files f ON o.file_id = f.id
  WHERE o.canonical_id = o.id AND f.ftype = 'file'`.
- Pull by queue position + `o.flags & Placed = 0` re-filter.
- Drop on success only.

Hardlinks/others keep their plain `id`-cursor listers (`list_out_tree_for_hardlinks`,
`list_out_tree_others`) — no size ordering needed — driven from inside the loop's `pull`
closure.

New count helper: `count_canonical_files_for_move()` (mirror `count_all_hashable_files`)
for the files-done bar total.

## 2. `copy_canonicals_to_source` → crossbeam pipeline

- W = `StrippedRecord` (the canonical file row); O = `Result<(FileId, bool),
  (FileId, Error)>` — identical to today's `results` vec element.
- Worker (`io_jobs` threads, owns a `Bytes` thread bar):
  - `check_between_files` between entries (`Err` → break, trailing `None`).
  - `content_id()?` → `src = extract_cache_dir/{cid}`, `dst = extraction_root/.sources/{cid}`.
  - `copy_single_file(fid, &src, &dst, shutdown, no_reflink, Some(&bar))` — the reflink
    path is instant; the sparse-copy path byte-increments the bar via the progress
    callback.
  - On `Ok` + `!keep_stage` → `fs::remove_file(&src)`.
  - `out.send(Some(outcome))`; `Err(_) => break`; trailing `out.send(None)`.
- Apply closure (main loop thread): per outcome
  - `Ok((id, is_copy))` → `set_file_flag(AtLinkSource)`, `set_file_flag(UsedRefLink, !is_copy)`,
  - `Err((id, Error::FileStat(e)))` → `recorder.record_file(id, PlacePhase, e)` +
    `set_file_flag(ErrorWhilePlacing)`,
  - `Err(Error::Interrupted)` → skip,
  - any other `Err` → panic (invariant), mirroring `ingest_hash_outcome`'s pattern.
  `bump` the files-done bar by the batch (`Count`, see Progress). No ingest helper needed.
- `recorder.flush()` once after the loop.

## 3. Progress design (user's rules)

Stage has **four** consumers of visual progress now; the phase bar total stays
`count_out_tree_rows`, the global stays `table_size * multiplier`.

| consumer | phase | global | own bars |
|---|---|---|---|
| `prepare_extraction_dir` | +1/dir | +1/dir | — |
| `materialize_link_tree` (unchanged) | +1/entry | +1/entry | — |
| `materialize_files` (copy) | +1/file done | +1/file done | worker `Bytes` thread bars (copy bytes) |
| `materialize_hardlinks` / `materialize_others` (single actions) | +1/done | +1/done | worker `Count` thread bars (or omit) |
| `copy_canonicals_to_source` | **none** | **none** | worker `Bytes` thread bars (copy bytes) **+ separate `Count` bar for files done** |

So:
- **Single actions** (hardlink, symlink, fifo, chardev, blockdev): worker does the
  action then `rt.progress.inc_both(1)` (+ its `Count` thread bar `inc(1)`).
- **Copy operations** (`materialize_files`): worker byte-increments its `Bytes` thread
  bar during the copy, and does `rt.progress.inc_both(1)` once the copy completes.
- **`copy_canonicals_to_source`**: worker byte-increments a `Bytes` **thread counter**
  (per-worker copy progress); a separate **files-done `Count` bar**
  (`push_sub_bar("moved", BarKind::Count)`, total = `count_canonical_files_for_move()`)
  is incremented once per completed file. **No `inc_both` / `inc_global`** — the phase bar
  and global are incremented exclusively by `materialize_link_tree` (or the
  materialize_* functions); a canonical file is counted once there, never twice.

All outer `rt.progress.inc_both(n)` calls in the four converted functions are removed.
`recorder.flush()` moves from per-batch to once after each loop.

## 4. `materialize_loop(rt, recorder, kind)` — merged materialize trio

```rust
enum MaterializeKind { Files, Hardlinks, Others }

struct MaterializeWork {
    canonical: FileRecord,                 // superset: others need major/minor/link_dst
    out: OutTreeRecord,
    out_canon: Option<OutTreeRecord>,      // hardlinks only: the placed canonical
}
```

- Public wrappers `materialize_files` / `materialize_hardlinks` / `materialize_others`
  stay as 3 thin call sites (keep `run()` and any callers/tests unchanged). Each builds
  its bar pool (`create_thread_bars` + `drop_thread_bars`) and calls the private loop.
- `pull` closure: `match kind` →
  - Files: `pull materialize_queue` (size-DESC) → `(FileRecord, OutTreeRecord, None)`,
  - Hardlinks: `list_out_tree_for_hardlinks(last_id, chunk)` → `(f, canon_out, out)`,
  - Others: `list_out_tree_others(last_id, chunk)` → `(e, out)`.
- Worker template (shared across kinds):
  ```
  check_between_files else break
  target_path = out.abs_path                    (files/hardlinks/others)
  check_path(config, <path>, canonical.to_stripped()) →
    Err(e)            => outcome Err((out.id, FileStat(e)))
    Ok((false, c, r)) => outcome Ok(not-placed)
    Ok((true, c, r))  => match kind {
        Files     => copy_single_file(... Bytes bar ...)
        Hardlinks => fs::hard_link(out_canon.abs_path, target_path)
        Others    => build_other(&canonical, &out, recreate_none_file_entries)
      } → outcome Ok(placed)  | on per-file error Err((out.id, Error))
  ```
  `check_path`'s `(path, record)` pair differs per kind (files check the cache source,
  hardlinks/others the destination) — folded into each kind's arm.
- `apply` = `process_results(take(pending), recorder, db,
  is_hardlink = matches!(kind, Hardlinks), set_reflink = matches!(kind, Files))` —
  the existing helper is reused verbatim; progress already happened in the workers.

## 5. Loop wiring

- Channels `bounded::<W>(WORK_CAPACITY)` / `bounded::<Option<O>>(OUT_CAPACITY)`,
  `FEED_CHUNK`/`DRAIN_CHUNK` constants as in hash; workers named
  `place-worker-{i}`; trailing `None` exit marker.
- `send_receive_loop(shutdown, send, recv, handles, DrainChunk, false, pull,
  |_| Ok(()), || Ok(true), apply)` — join + drain-to-empty endgame gives the same
  race-free `drop(recv)` as hash/rehash. `io_jobs` = pool size everywhere.

## 6. Interrupt semantics (unchanged)

`check_between_files` between entries; force aborts mid-copy via the
`sparse_copy_with_progress` callback's `check_in_flight`. Queues are **not** dropped on
interrupt. Known accepted edge: on a mid-run interrupt, a copied-then-removed cache file
whose `AtLinkSource` flag never got applied would be re-copied on resume with a missing
source → `Errored`. Acceptable (the Place phase is not mid-phase-resumable today); noted.

## Files touched

| File | Change |
|---|---|
| `db/place.rs` | + `create/populate/pull/drop_canonical_move_queue`, + `create/populate/pull/drop_materialize_queue`, + `count_canonical_files_for_move`; keep the existing listers |
| `db.rs` | facade delegations for the new fns |
| `unarchive/place.rs` | rewrite `copy_canonicals_to_source`; merge the materialize trio into `materialize_loop(…, kind)` + 3 thin wrappers; inner-loop progress + thread bars; `copy_single_file` gains a `pb: Option<&ProgressBar>` byte-progress hook |
| `plans/place-crossbeam.md` | this plan; as-built filled at implementation time |

## Open notes / follow-ups

- `status_message_rebuilding` call in `run()` stays as-is (log only).
- A place test battery (worker per-kind outcome mapping, queue ordering/pull filters,
  copy-remove-source semantics, interrupt mid-copy) is deferred to a follow-up per user
  instruction ("no testing yet").