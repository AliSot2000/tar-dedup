# Plan: cross-phase DB sanity checks

Status: **agreed (2026-10-06).** All open questions settled:
- run **every** invariant as its own `count_*` returning the number of offending rows (check per case);
- rehash: explicit verdict flag `RehashVerified` (option a);
- files-table placement checks: `E8a` (AtLinkSource, unscoped) + `E8b` (`canonical_id = id` scope);
- tolerate-able cases are their own checks with warn/force tiers — no combined checks.

## Goal

Run a DB integrity battery **between every phase** of both pipelines to catch
programming errors or user-manipulated/inconsistent data. Checks assume the
phase they run after **has completed successfully**; they never run on the
interrupt path. Archive invariants are **additive** — what held after an archive
phase must still hold at every later archive phase, and (as a config-free
subset) during extraction.

## Behavior changes (prerequisites)

### B1 — scan promotes everything to `archived` → `unarchived`
> **Superseded by `plans/024-scan-end-unarchived.md`** (2026-10-06): the blanket promote
> became a principled end-of-scan gate with cause-bucketed remainders and a
> force/panic split. Kept here for history.

`unarchive/scan.rs::run_inner`, after `report_scan_completeness`: if any rows
remain in `phase = 'archived'` (extracted-but-unconfirmed, never-extracted, or
non-file rows that today stay `archived`), then:
1. emit `tracing::info` with the leftover count;
2. `Recorder::record_session(ScanTar, …)` a session error (count);
3. blanket promote via a new `db/scan.rs::promote_remaining_to_unarchived(conn)`
   (`UPDATE files SET phase='unarchived' WHERE phase='archived'`).

This makes `E3` (`count_in_phase(Archived) == 0`) sound. Existing missing-payload
handling is unchanged (error unless `--force`); truncation salvage keeps working
(leftover cache payloads are hashed again in rehash). `list_materialized_entries`
is phase-agnostic, so the out_tree build is unaffected.

### B2 — `ErrorWhileExtracting` becomes real (force-gated)
`unarchive/scan.rs::process_entry` payload-copy failure:
- `--force` → set `FileFlag::ErrorWhileExtracting`, `record_file_scan_error`,
  `tracing::warn`, **continue** to the next payload;
- else → record + return error (today's behavior).

Makes `E1` (`AppendedPath ⟹ FileExtracted OR ErrorWhileExtracting`) meaningful.

### B3 — `RehashVerified` verdict flag
- New `FileFlag::RehashVerified = 25` (`db/flags.rs`, bit 25 free — `MAX_FLAG_BIT` 62).
- `ingest_rehash_outcome` (`db/rehash.rs`) `Match` arm also sets
  `RehashVerified` (one line + doc comment on the variant).
- Contract: every elected row ends `rehashed` with **exactly one** of
  `RehashMismatch | ErrorWhileRehashing | RehashVerified`.

## Invariant functions (`db/integrity.rs`)

Each invariant is a dedicated `pub fn count_<name>(conn, …) -> Result<u64>`
returning the number of offending rows. Two aggregators assemble them and apply
the per-check tier:

```rust
pub struct ArchiveGate {
    pub eager_filter: bool,
    pub detect_hardlinks: bool,
    pub sparsify: bool,   // config.sparse.sparsify
    pub min_pages: u64,
}

pub fn check_after_archive_phase(conn, gate: &ArchiveGate, phase: PipelinePhase) -> Result<()>;
pub fn check_after_extract_phase(conn, config: &ExtractConfig, phase: ExtractPipelinePhase) -> Result<()>;
```

`check_after_extract_phase` **also runs the archive invariants** (config-free
subset) before the extract-specific ones; passes a `sparsify:false` gate on the
extract side. `ArchiveGate` is built from `ArchiveConfig` at the call site.

New `Database` facade entries delegate to the `count_*` fns; the orchestrators
call the aggregators through the facade.

### Archive battery (also partially on extract)

| # | Invariant (violation = count > 0) | Scope / gate | Tier |
|---|---|---|---|
| A1 | hashable rows: `ErrorWhileHash ⟺ sha1 IS NULL` (sha1 is the sole fail marker; `sparse_count` leg dropped — schema defaults it to 0) | `ftype='file'` ∩ archive-filter(if `eager_filter`) ∩ `FileHardlinkCanonical`(if `detect_hardlinks`) | panic |
| A2 | `ErrorWhileHash` AND `sha1 IS NOT NULL` (mutual exclusion) | all rows (config-free) | panic |
| A3 | `sha1 IS NULL ⟹ canonical_id IS NULL` | all rows (config-free) | panic |
| A4 | `CheckWithCanonicalCompleted` count == 0 | all rows (reuse `dedup::count_check_with_canonical_completed`) | panic |
| A5 | eligible ∧ `canonical_id IS NULL` ⟹ `ErrorWhileDedup` | eligible = `ftype='file'` ∩ `sha1 NOT NULL` ∩ ¬`ErrorWhileHash` ∩ archive-filter-passed | panic |
| A6 | sparsify candidates have `HasSparse XOR ErrorWhileSparsify` | gate `gate.sparsify`; `canonical_id=id` ∩ file ∩ `sparse_count >= min_pages` ∩ archive-filter | panic |
| A7 | `canonical_id = id ⟹ AppendedPath XOR ErrorWhileArchive` | all rows (config-free) | panic |
| A8 | `count_in_phase(Staged) == 0` | config-free (promote the `end_session` full-close assert here) | panic |

Config-free subset run on extract = A2, A3, A4, A5, A7, A8. A1/A6 skipped
(their gates need the archived archive config, which `normalize_installed_catalog`
clears).

### Extract battery

| # | Invariant | Scope / gate | Tier |
|---|---|---|---|
| E1 | `AppendedPath ⟹ FileExtracted OR ErrorWhileExtracting` | canonical rows (`canonical_id=id`); extends `count_missing_payloads`, subtracting rows carrying `ErrorWhileExtracting` | warn (`--force` semantics; scan is the real gate) |
| E2 | `FileExtracted OR ErrorWhileExtracting ⟹ AppendedPath` | canonical rows (stray cache / truncation salvage) | warn |
| E3 | `count_in_phase(Archived) == 0` | enabled by B1 | panic |
| E4 | every elected row has **exactly one** of `RehashMismatch \| ErrorWhileRehashing \| RehashVerified`; and no rehash flag on non-elected rows | gate `config.scan.rehash` | panic |
| E5 | non-dir `out_tree` rows: parent path present as a dir `out_tree` row | `(flags & IsDir)=0`; parent via SQLite dirname trick | panic |
| E6 | non-dir `out_tree` rows: `file_id IS NOT NULL` | `(flags & IsDir)=0` | panic |
| E7 | `(flags & IsDir) != 0` AND `file_id IS NOT NULL` ⟹ referenced `files.ftype = 'dir'` | file_id NOT NULL rows | panic |
| E8a | files: `AtLinkSource ⟹ FileExtracted` | files table, unscoped (only link-tree moves set it) | panic |
| E8b | files: `canonical_id = id AND (UsedRefLink OR ErrorWhilePlacing) ⟹ FileExtracted` | files table (dup rows exempt — their flag noise is their own `out_tree` outcome) | panic |
| E9 | `out_tree`: `ErrorWhilePlace ⟹ NOT (Placed \| RemovedPrevious \| Conflict \| IsHardlink \| UsedRefLink)` | holds by construction today — **no phase code change needed** | panic |
| E10 | placed canonicals get `AppliedMetadata XOR ErrorWhileApplyingMetadata` | link-tree: `AtLinkSource`; else non-dir ∩ `Placed` ∩ `canonical_id=id`; dirs only under `--overwrite-dir` | panic |

Notes:
- `E6` — depends on `ensure_parent` only ever injecting dir rows with `NULL`
  `file_id`; every non-dir `out_tree` row is built from a `Some(r.id)` record.
- `E8b` origin — `apply_flags_to_files` (`db/place.rs:667`) aggregates
  `UsedRefLink`/`ErrorWhilePlacing` onto **all** referencing files incl. dups in
  non-link-tree mode; dups never carry `FileExtracted`. Canonicals always do, so
  a canonical with these flags but no extraction is a tamper/bug signal.
- `E9` — the "error ⟹ none of the trace flags" formulation (the arrow is
  inverted from the original draft); `ingest_materialize_results` /
  `ingest_results_link_tree` error arms set only `ErrorWhilePlace`.

## Wiring

```rust
// archive.rs — inside the phase loop, after `run_phase(state.phase, &rt)` returns Ok:
verify_after_phase(&rt, state.phase)?;   // new

// archive.rs
fn verify_after_phase(rt: &ArchiveRTArgs, phase: PipelinePhase) -> Result<()> {
    let gate = ArchiveGate { eager_filter, detect_hardlinks, sparsify, min_pages };
    let bar = rt.progress.begin_integrity(archive_phase_check_count(&gate, phase));
    let tick = || bar.inc(1);
    rt.db.integrity_check_after_archive(&gate, phase, Some(&tick))
}

// unarchive.rs — after the ScanTar arm returns the db, and after each later arm:
verify_after_phase(&rt, state.phase)?;   // new

// unarchive.rs
fn verify_after_phase(rt: &ExtractRTArgs, phase: ExtractPipelinePhase) -> Result<()> {
    let bar = rt.progress.begin_integrity(extract_phase_check_count(rt.config, phase));
    let tick = || bar.inc(1);
    rt.db.integrity_check_after_extract(rt.config, phase, Some(&tick))
}
```

`check_after_extract_phase` internally calls the archive invariants (config-free
subset) first, satisfying "the archive checks also apply during extraction".
`Debug`/Release note: checks are always-on (not gated behind `debug_assertions`).
Both aggregators take an optional `&dyn Fn()` progress callback fired once per
completed check; `begin_integrity(total)` (progress.rs) draws the
`integrity checks: {pos}/{len}` sub-bar during the battery, sized with
`archive_phase_check_count` / `extract_phase_check_count` which mirror the exact
gating of the aggregators.

**Phase gating** (which checks are *established* by the completed phase):

| Check | Runs when the completed phase is |
|---|---|
| A2, A3, A4 | every archive phase (+ extract) |
| A1 | `Hash` or later in pipeline order (both modes) — **not** `Filter`, since in eager mode filter precedes hash |
| A5 | `Dedup` or later |
| A6 | `Sparsify` or later (only when the archive ran sparsify) |
| A7, A8 | `Archive` |
| E1, E2, E3 | every extract phase after scan |
| E4 | `Rehash` or later (only when `scan.rehash`) |
| E5, E6, E7 | `PlacementPrologue` or later |
| E8a, E8b, E9 | `Place` or later |
| E10 | `Permissions` (only in the toggled scope) |

## Tests (module-inline, real `schema::initialize`)

In `integrity.rs` `#[cfg(test)]`, for each `count_*`:
- clean DB → `== 0`;
- one intentionally violating row → `== 1`;
- aggregator panics on violation (`catch_unwind`);
- `progress_ticks_match_phase_check_count` — the tick total equals the bar-length
  count function per phase.

Behavior-change tests:
- B1: `promote_remaining_to_unarchived` flips only `archived` rows; scan end
  with a leftover `archived` row promotes + records a session error (scan-level).
- B2: `process_entry` under `--force` sets `ErrorWhileExtracting`, records the
  file error, keeps going; without `--force` returns the error (scan-level).
- B3: `ingest_rehash_outcome` sets `RehashVerified` on `Match`; E4's
  exactly-one-or-nothing math on a seeded matrix.

Reuse existing seed helpers (`filter_reason` −1 internal include rows, `-1`
include / `5` exclude matrix from `db/rehash.rs` tests).

## Implementation order

1. `B3` (`RehashVerified` flag + ingest) — smallest, unblocks E4.
2. `B1` + `B2` (scan changes + `promote_remaining_to_unarchived`).
3. `integrity.rs` `count_*` fns + `check_after_archive_phase` /
   `check_after_extract_phase` + `Database` facade entries.
4. `verify_after_phase` wiring in `archive.rs` / `unarchive.rs`.
5. Module tests + build:
   `cargo build -p tar-dedup` then `cargo test -p tar-dedup --lib` (dev profile only).

## Open items (resolved)

- E1/E2 are their own warn-tier checks (tolerated cases get dedicated checks,
  no combined counting).
- Out-tree `file_id` ↔ ftype check is one-directional per E7.
- Placement error-trace bookkeeping needs no phase-code change (E9 holds today).
