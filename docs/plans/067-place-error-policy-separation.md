# Plan: place error/policy separation (extract materialize loop)

## Problem
`materialize_loop` conflates two unrelated axes in `MaterializeOutcome` and
`check_path`:

- **Real failures** — `FileStatError::{Io, Nix, Copy, General}` from stat/read/
  copy/link/mknod/remove_file — genuinely belong in the errors table, set
  `ErrorWhilePlace`, and trigger `fail_fast` abort.
- **Policy decisions** — the `check_path` judges `skip-existing`, `too-old`,
  `too-new`, `type-mismatch` — are *the user's requested conflict policy*, not
  errors. Today, in non-`silent_conflicts` mode they are promoted to
  `Err(FileStatError::General{..})`, recorded into `errors`, set
  `ErrorWhilePlace`, and can `fail_fast`-abort. That pollutes
  `count_errors`/`--db-retention on-error`/`--no-errors` with non-errors.

Goal: separate the two. Real errors are returned **and** recorded; policy judges
become `conflict` outcomes with no recorded error.

## Locked decisions
1. Only the four `check_path` policy judges become outcomes. The
   `build_other` `"Missing major and/or minor"` `General` stays a real error
   (returned + recorded + `fail_fast`-aborting).
2. Assessment-era io errors (can't stat existing; can't read its mtime;
   `remove_file` fails) → **favor the existing file**:
   `{ placed:false, conflict:true }` **plus** the `FileStatError` carried with
   the result.
3. `fail_fast` aborts on real errors including the maj/min `General`; policy
   skips never abort.
4. Skips are not recorded (do not affect `count_errors` / `--db-retention
   on-error` / `--no-errors`).
5. Real errors always surfaced + `tracing::error!`. Policy judges →
   `tracing::debug!` when `!silent_conflicts`, nothing when silent.

## Channel semantics (corrected)
`None` enqueued on the materialize result channel is **only the worker-shutdown
sentinel**, sent once when a worker exits its loop.

Therefore `materialize_one` may **not** use `None` to signal an interrupt.
`Error::Interrupted` is carried as a **value** in the outcome
(`error: Some(Error::Interrupted)`) and is handled **inside ingest**: ingest
skips it (sets no flags), so the in-flight row remains `pending` and is retried
on resume. This preserves the "in-flight row must not be committed" guarantee
without overloading the sentinel.

## Design
New outcome type (flags meaningful even on error):

```rust
struct MaterializeOutcome {
    result: MaterializeResult,          // id / placed / conflict / removed / used_copy
    error: Option<Error>,               // Some(FileStat) = real error; Some(Interrupted) = force-abort
}
```

- **`check_path`** → `((bool, bool, bool), Option<FileStatError>)`: always a
  conclusion + optional assessment error.
  - policy judges → `((false, true, ...), None)` + `debug!` / silent.
  - assessment io (metadata/mtime/`remove_file`) → `((false, true, _), Some(Io))`;
    `remove_file`-fails → `removed:false` (fixes small bug).
  - `!exists`/writable/remove-due-to-replace → unchanged.
  - the four `Err(FileStatError::General{..})` branches are **deleted**.
- **`materialize_one`** → returns `MaterializeOutcome` (never `None`):
  - `Some(Error::Interrupted)` instead of the old `None`.
  - assessment error / `!writable` → `{ placed:false, conflict, removed }` + `Some(e)`.
  - writable → copy/link/special: success → placed + `None`; failure → write error
    folded into `error` (flags carry `check_path`'s conflict/removed); maj/min
    stays `Some(General)`.
- **`materialize_worker`**: drops the `None => break` arm from
  `materialize_one`; still stops on `shutdown.is_interrupted()` or closed work
  channel; still sends `out.send(None)` once when the loop exits.
- **`process_results`** & **`ingest_materialize_results`** → `&[MaterializeOutcome]`.
  - ingest (one txn): `None` → set Placed/Conflict/Removed from `result`;
    `Some(FileStat(_))` → set flags + `ErrorWhilePlace`;
    `Some(Error::Interrupted)` → **skip (row stays pending)**;
    other → panic invariant.
  - `process_results` (post-ingest): `Some(FileStat(e))` →
    `recorder.record_out_tree(result.id, ERROR_PHASE, e, …)` + `fail_fast` abort
    (catches Io/Nix/Copy/maj-min General); `Some(Interrupted)` → skip.

## Files
- `crates/tar-dedup-lib/src/unarchive/place.rs`
  (`MaterializeOutcome`, `materialize_worker`, `materialize_one`,
   `process_results`, `check_path`, channel bound, `run` unaffected).
- `crates/tar-dedup-lib/src/db/place.rs` + `db.rs` facade
  (`ingest_materialize_results` new shape).

## Tests (place.rs + db/place.rs `#[cfg(test)]`)
- Re-shape `Result<MaterializeResult,(id,Error)>` based tests
  (`process_results_*` ~2258–2345, `check_path_type_mismatch_loud_errors` 1791,
  `materialize_worker_force_breaks_and_sends_none` 2404) to the new shapes.
- `check_path` tests: policy judges → `Ok+None`+debug; assessment-io → `Ok+Some(Io)`;
  `check_path_type_mismatch_loud_errors` now expects an outcome + debug, not `Err`.
- `materialize_one_*`: `conflict_keeps_existing` / `copy_error_records`
  (still records), `force_drops_outcome` → now emits `Some(Interrupted)`.
- New: policy skip not recorded (buffered 0) yet sets `Conflict`; inject
  `Some(Interrupted)` through ingest → row stays pending.
- New: `fail_fast` aborts on the maj/min `General`.

## Implementation status (done)
- `MaterializeOutcome { result, error }` defined in `db/place.rs`; `MaterializeResult`
  gains `Debug/Clone/PartialEq/Eq`.
- `check_path` → `((bool,bool,bool), Option<FileStatError>)`; the four policy judges
  now return a `(false,true,..)` conclusion (no error, `debug!`/silent);
  assessment io errors (`symlink_metadata`, mtime, `remove_file`)
  → `(false,true,removed)` + `Some(Io)` (favor existing, `removed:false` on
  `remove_file`-fail per the small-bug fix). `build_other` maj/min stays a real error.
- `materialize_one` → `MaterializeOutcome` (never `None`); force-abort mid-copy emits
  `Some(Error::Interrupted)`, which `ingest_materialize_results` skips (row stays
  pending). The channel `None` remains worker-shutdown-only.
- `materialize_worker` always sends the outcome (dropped the old `None => break` from
  `materialize_one`); stops on shutdown / closed work channel.
- `process_results` takes owned `Vec<MaterializeOutcome>`: ingest all results, then
  record + `fail_fast` on `Some(FileStat)` (catches Io/Nix/Copy/maj-min).
- `db.rs` facade + `db/place.rs::ingest_materialize_results` updated to the new shape;
  flag-constellation validator runs only for clean (non-error) outcomes, so error
  rows may carry any `{placed,conflict,removed}` combination.
- Notable behavior change: `determine_file_type` failure now → favor-existing +
  recorded `Io` for **both** silent and loud (previously loud errored, silent
  proceeded with the fallback ftype). Consistent with "when in doubt, keep existing".
- Tests reshaped; new coverage: policy skip not recorded yet sets `Conflict` (via
  `check_path`/`materialize_one` tests), `Interrupted` through ingest leaves pending
  (`process_results_ignores_interrupted`), `fail_fast` on maj/min.
- Verified: `cargo test -p tar-dedup-lib --lib` = 722 passed / 0 failed;
  `cargo build --workspace` clean (only pre-existing `error_guarded`/`dedup.rs`
  warnings, untouched).