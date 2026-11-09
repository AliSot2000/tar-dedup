# 085 — sound extract start resolution (WorkPresence::Done, drop dead redundancy)

Status: **approved — implementing.**

## Motivation

`unarchive::run`'s start-resolution block is redundant and unsound:

1. The refactored `(work, state)` tuple (lines 61-75) is **dead**: line 77 rebinds `state` via
   `load_extract_state`, and lines 86-92 rebind `work` from third-party `is_file` checks. Three
   overlapping `db_path().is_file()` gates compute the same thing.
2. `WorkPresence` has no `Done` variant: a present db whose `phase == Done` reports
   `WorkPresence::Absent` (lying that there is no work).
3. `product` is hardcoded `ProductPresence::Absent` and never recomputed, so `resolve_start`'s
   `(Absent, Finished)` refusal is unreachable on extract.

## 1. `common/start.rs` — `WorkPresence::Done` + `Option<ProductPresence>`

Add variant `Done` to `WorkPresence`.

`resolve_start(policy, work, product: Option<ProductPresence>)`:

- `Fresh` → `RunFresh`.
- `Resume` → `Incomplete` ⇒ `Resume`; `Absent`/`Done` ⇒ `Err` (no incomplete work to resume).
- `Create`:
  - `Incomplete` ⇒ `Err` (use `resume`/`--fresh`).
  - `Done` ⇒ `Err` `"already extracted; use --fresh to start over"` (mirrors archive's
    finished-product refusal; `Done` is a real terminal state).
  - `Absent` + `Some(Finished)` ⇒ `Err` (archive-only product refusal).
  - `Absent` + `Some(Absent) | None` ⇒ `RunFresh`.

Add a block comment explaining the `Option` semantics: `Some` = archive product (footer /
bypass catalog presence), `None` = extract (no finished-archive concept; `Done` covers it).

`archive.rs` passes `Some(product)` (already computed); `unarchive.rs` passes `None`.

## 2. `unarchive.rs` — single work resolution, drop dead helpers

Replace lines 61-92 with one `(work, state)` resolution keyed on present db + extract phase:

```rust
let (work, state) = if config.paths.db_path().is_file() {
    let db = Database::open(&config.paths.db_path())?;
    match db.load_extract_runtime_state()? {
        None => (WorkPresence::Absent, ExtractRuntimeState::new()),
        Some(ert) if ert.phase == ExtractPipelinePhase::Done => (WorkPresence::Done, ert),
        Some(ert) => (WorkPresence::Incomplete, ert),
    }
} else {
    (WorkPresence::Absent, ExtractRuntimeState::new())
};
```

This subsumes `db_extract_phase_absent`/`fresh_bypass`: a fresh bypass product's baked catalog has
no extract-phase meta → `None` → `Absent`, exactly the `fresh_bypass` intent.

- Delete `db_extract_phase_absent` (lines 39-45) and `fresh_bypass` (lines 79-84) and the second
  `work`/`state` computation (lines 77, 86-92).
- `product` becomes `None` for extract (drop line 53's hardcoded const).
- Delete `load_extract_state` (lines 279-288) and its test `load_extract_state_defaults_when_db_absent`.
- Move the resolution into a small helper `resolve_extract_work(db_path) -> Result<(WorkPresence, ExtractRuntimeState)>`
  plus a unit test (absent db ⇒ Absent/new; present db no meta ⇒ Absent/new; present db
  `phase != Done` ⇒ Incomplete; present db `phase == Done` ⇒ Done).

## 3. Touch-points

- `archive.rs:18,39-47,68` — product already `Some`-shaped via `Option` change; adjust the call.
- `crates/tar-dedup` bins unaffected (they call `run`).

## 4. Verification

- `cargo test -p tar-dedup-lib --lib`.
- VM: sync, `cargo test --features test-all`.
- (Note: `unarchive.rs` is mid-rework by the author; ask before running tests.)