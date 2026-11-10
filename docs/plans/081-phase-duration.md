# 081 — Per-phase timing (INFO) via a shared `timed` helper

Status: **approved — implementing (2026-11-10).**

## Motivation

The author wants each pipeline phase to report how long it took at `info` level,
so a run's log shows per-phase durations. Earlier work on this (phase-duration
(081): per-phase INFO timing of progressbar create/destroy) was stashed and
**dropped** — that approach timed the progressbar scaffolding, not the phase
runs. This plan supersedes it.

## Decided design points

1. **Shared helper in `common.rs`** — one `timed` function wraps any closure,
   records `Instant` before/after, and logs the elapsed duration:
   ```
   fn timed<R>(func: impl FnOnce() -> R, log_info: &str) -> R {
       start = Instant::now()
       let res = func()
       tracing::info!("{log_info} took: {}", format_elapsed(start.elapsed()))
       res
   }
   ```
   - Uses the existing `common::formatting::format_elapsed` for the duration
     (`MM:SS` / `H:MM:SS`), matching the rest of the codebase.
   - `Instant` (monotonic) rather than wall-clock `datetime` — durations, not
     timestamps.
   - The result of the closure is returned unchanged, so call sites keep their
     `?`/match shape.
2. **Wrap all phase runs** in both pipelines with `timed`, passing the phase
   name as the label (`{phase} took: …`):
   - `archive.rs` — the `run_phase(&state.phase, &rt)` dispatch in the phase
     loop.
   - `unarchive.rs` — each phase arm in the extract loop: scan, filter, rehash,
     prologue, place, permissions, cleanup.
   - The cross-phase `verify_after_phase` integrity battery and the
     progressbar/`enter_phase` scaffolding are **not** wrapped — only the phase
     execution itself.
3. **Label**: the phase's own name string (`PipelinePhase::as_str()` /
   `ExtractPipelinePhase::as_str()`), e.g. `hash took: 1:05`, `place took: 0:02`.

## Edit map

- `crates/tar-dedup-lib/src/common.rs` — add `pub fn timed` (+ `Instant` import).
- `crates/tar-dedup-lib/src/archive.rs` — wrap the `run_phase` match arm call.
- `crates/tar-dedup-lib/src/unarchive.rs` — wrap the seven phase arms.
- `docs/plans/081-…` — this plan.

## Order

1. `common.rs` helper → 2. `archive.rs` → 3. `unarchive.rs` → 4. verify.

## Verification

- `cargo test -p tar-dedup-lib --lib` stays green (no behavior change; the
  wrapper only times + logs).
- `cargo test -p tar-dedup` integration stays green.
- Log output shows `info` lines like `hash took: 0:05` for every phase.
