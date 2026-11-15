# 092 — Phase time ledger

Status: **implemented (2026-11-13).**

Append-only per-phase timing ledger stored under one `meta` key, so archive and
extract phase runtimes can be compared across attempts and resumes without
starting/stopping the tool thousands of times.

## Format

`meta.key = "time_ledger"`, value = JSON array:

```json
[
  {"phase":"archive::hash","event":"start","at":"2026-11-13T14:20:17.123Z"},
  {"phase":"archive::hash","event":"stop","at":"2026-11-13T14:21:02.456Z"}
]
```

- `phase` is the lower-case pipeline/phase slug: `archive::inventory`,
  `archive::hash`, `extract::scan_tar`, `extract::placement_prologue`, …
- `event` is `start`, `stop`, `interrupt`, or `error`.
- `at` is RFC3339 UTC (sub-second precision kept; this is a benchmark, unlike
  the whole-second cleanup DB name).
- Pairing rule: a `start` is closed by the next non-`start` event for the same
  label. An unmatched `start` is an accepted best-effort gap (hard kill,
  interrupt before the scan installed a DB, etc.).

Additive key only: no schema change, no `DB_VERSION` bump, no migration.

## Rust surface

### `db/meta.rs`

```rust
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum LedgerEvent { Start, Stop, Interrupt, Error }

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct TimeLedgerEntry {
    pub phase: String,
    pub event: LedgerEvent,
    pub at: DateTime<Utc>,
}
```

- `MetaKey::TimeLedger => "time_ledger"` (added to `as_str`, `parse`, `all`).
- `MetaEntry::TimeLedger(Vec<TimeLedgerEntry>)`, JSON encode/decode.
- `get_time_ledger(conn) -> Result<Option<Vec<TimeLedgerEntry>>>`.
- `append_time_ledger(conn, &entry)` — read, push, upsert.
- `dump_meta` includes the ledger automatically.

### `config/phases.rs`

`PipelinePhase::ledger_label()` → `format!("archive::{}", as_str())`;
`ExtractPipelinePhase::ledger_label()` → `format!("extract::{}", as_str())`.
Centralizes the label so it cannot drift from the persisted phase slug.

### `db.rs`

- Re-export `LedgerEvent`, `TimeLedgerEntry`.
- `time_ledger()` → current entries (empty when absent).
- `record_phase_event(phase, event, at)` — **best effort**: one transaction; on
  failure logs `warn!("time ledger: …")` and returns. Timing must never abort a
  pipeline run.

## Where events are written

### Archive (`archive.rs` loop)

```
enter_phase
record Start(phase.ledger_label(), now)
match timed(|| run_phase(...)):
    Ok  -> if phase != Archive: record Stop(now)
    Err(Interrupted) -> record Interrupt(now)
    Err(e)           -> record Error(now)
```

`Archive` is the exception: the body records `Stop` itself so the footer
snapshot contains the complete ledger *before* the archive integrity hash.

### Archive body (`archive/tar_builder.rs`)

- `end_session`: immediately after `writer.finalize(write_tar_eof)` returns
  `Ok`, capture `now`; after session/byte bookkeeping record `Stop` when
  `write_tar_eof` is true, **before** the existing `rt.db.checkpoint()` and
  `write_footer`. Graceful-interrupt finalization (`write_tar_eof == false`)
  must not record `Stop`; the loop records `Interrupt`.
- `run_bypass_archive`: record `Stop` before the final `rt.db.checkpoint()` so
  it lands in the baked catalog that becomes `manifest.sqlite`.
- `run_footer_one_shot` (resume where the stream finished but the trailer was
  interrupted): record `Stop` after `write_footer` succeeds (work-DB only; the
  footer was built before this point).

The outer loop skips `Stop` for `Archive`; the body owns it on every `Ok` path.

### Extract (`unarchive.rs` loop)

- `started_at = Utc::now()` at loop top, always.
- `db.is_some()` → record `Start` immediately.
- `db.is_none()` (fresh footer-less scan) → defer. On success, once the scan
  installed a DB, write `Start(started_at)` then `Stop`. Interrupt before a DB
  exists → no entry (accepted).
- `Ok` → `Stop`; `Err(Interrupted)` → `Interrupt`; `Err(e)` → `Error`, before
  the state save/checkpoint.
- `Cleanup` `Stop` after cleanup is best effort: it survives with
  `--db-retention always`/`on-error`, and is lost when cleanup deletes the DB.

## Archive integrity timing (log only)

The integrity hash is **not** part of the ledger; it is logged via the existing
`timed` helper:

- `archive_footer::write_footer`: wrap `hash_archive_bytes(...)` with
  `timed(..., "archive integrity")`. This runs after the ledger `Stop`.
- `unarchive/scan.rs`: wrap the `verify_archive_integrity(...)` call with
  `timed(..., "archive integrity")` at the start of scan.

## Caveats / behavior

- Extract work DBs inherit the archive ledger from the footer/manifest, so
  labels are namespaced (`archive::*` vs `extract::*`).
- A fresh run (`--fresh`) wipes or re-copies the work DB, so its ledger resets
  accordingly.
- The complete archive ledger (including `archive::archive` `Stop`) is in the
  footer; the complete extract ledger requires `--db-retention always` (or
  `on-error` + an error), since a clean run deletes the work DB.
- Ledger write failures warn and never fail the run.

## Tests

- `db/meta`: append/round-trip all four events, order preserved, exact labels,
  `MetaKey::all`, `dump_meta` includes `time_ledger`.
- Archive run with `db_retention = Always`: retained DB has
  `archive::inventory` and `archive::archive` `Start`/`Stop` pairs with
  `stop >= start`.
- Footer carries the `archive::archive` `Stop`.
- Bypass round-trip: manifest/work DB has `archive::archive` `Stop` and
  `extract::place` `Start`/`Stop`.
- Optional: reuse an interrupt test and assert an `Interrupt` entry.

## Verification

1. `cargo test -p tar-dedup-lib --lib`.
2. Bypass round-trip and archive-footer subsets.
3. Full `--features test-all` on the VM once it is online.
