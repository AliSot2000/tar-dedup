# 086 — Centralize DB checkpoints in the orchestrators

Status: **approved — implementing (2026-11-11).**

## Motivation

Work-db checkpoints (WAL TRUNCATE) are currently scattered: some live in the
orchestrators, some inside phase `run` functions. The pipeline loops own the
phase machine — only they know when a phase transition happens and when an
interrupt aborts a phase. This plan moves all **phase-transition and
interrupt** checkpoints into `archive.rs::run` / `unarchive.rs::run`, makes
them unconditional (transition) or phase-gated (extract interrupt), and removes
the checkpoints that phase `run` functions took on themselves.

## Rules

1. **Transition:** once the phase is updated to the next (`save_*_runtime_state`),
   checkpoint the database — **unconditionally**, for every phase.
2. **Interrupt (archive):** on `Error::Interrupted`, after saving state,
   checkpoint — **unconditionally** (all archive phases; inventory restarts on
   resume, so a checkpoint is harmless).
3. **Interrupt (extract):** on `Error::Interrupted`, save state + checkpoint
   **only for `ScanTar` and `Rehash`** — the two extract phases designed for
   mid-phase resume. `PlacementPrologue`, `Place`, `Permissions` are
   **end-of-phase only** and would break if resumed on half-finished data.
4. **Phase `run` functions never checkpoint themselves** — except the
   `tar_builder` checkpoints, which are **correctness-required** (they precede
   raw `fs::copy`/`read_to_end` of the DB file into tar members / the footer,
   and cannot move to the loop).

## Current checkpoint locations

| # | File:line | Enclosing fn | Action |
|---|-----------|--------------|--------|
| 1 | `archive/tar_builder.rs:369` | `run_bypass_archive` | **KEEP** (bake DB into product) |
| 2 | `archive/tar_builder.rs:385` | `progress_abort_report` | **KEEP** (abort durability) |
| 3 | `archive/tar_builder.rs:435` | `force_abort_session` | **KEEP** (abort path) |
| 4 | `archive/tar_builder.rs:448` | `append_snapshot` | **KEEP** (raw `fs::copy` of DB) |
| 5 | `archive/tar_builder.rs:510` | `end_session` | **KEEP** (raw `read_to_end` for footer) |
| 6 | `unarchive/scan.rs:524` | `scan::run_inner` | **REMOVE** → orchestrator interrupt/transition |
| 7 | `unarchive/place_prologue.rs:58` | `place_prologue::run` | **REMOVE** → orchestrator transition |
| 8 | `unarchive.rs:200` | `unarchive::run` | **RESTRUCTURE** → unconditional transition + gated interrupt |

## Edit map

### `archive.rs` (`run`)
- Transition block (`:151-158`): after `db.save_archive_runtime_state(&state)?`,
  add `db.checkpoint()?` (unconditional).
- Interrupt arm (`:133-146`): after `db.save_archive_runtime_state(&state)?`,
  add `db.checkpoint()?` (unconditional).

### `unarchive.rs` (`run`)
- Transition block (`:188-195`): change the Place/Permissions-gated checkpoint to
  unconditional after `save_extract_runtime_state`.
- Cleanup arm (`:162-169`): after `save_extract_runtime_state(Done)` + before/after
  `cleanup_workdir`, checkpoint (transition to Done).
- **New interrupt arm:** the phase-dispatch `match` (currently no interrupt
  handling — phases propagate `Error::Interrupted` straight out). Add a match
  guard on the phase result: `Err(Error::Interrupted)` → save state, and
  `if matches!(completed, ScanTar | Rehash) { edb.checkpoint()? }`, then return
  `Ok(())` (clean early return, mirroring archive).

### `unarchive/scan.rs` (`run_inner`)
- Remove `d.checkpoint()?` at `:524`. Keep `save_extract_scan_state` +
  `flush_scan_recorder` (those are scan-progress persistence, not DB-file
  checkpoints); durability is now the orchestrator's job.

### `unarchive/place_prologue.rs` (`run`)
- Remove `rt.db.checkpoint()?` at `:58` (end-of-phase only → covered by the
  orchestrator transition checkpoint).

## Verification

- `cargo test -p tar-dedup-lib --lib` + `cargo test --features test-all` on host
  and VM (sync via tar-over-ssh, `TMPDIR=/home/test-user/tmp`).
- Interrupt smoke: kill mid-hash (archive) and mid-scan (extract) → resume
  continues from the persisted row; kill mid-place → prologue not re-run, place
  restarts cleanly.
