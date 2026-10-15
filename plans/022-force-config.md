# Plan: unify `force_scan` into `force`, move `force` into `ProcessOptions`

Status: **agreed (2026-10-05), implementation round.**

## Goal

- Merge `force_scan` (extract scan "no catalog" bypass) into `force`.
- Move `force` from `ExtractConfig` top-level into the shared `ProcessOptions`.
- Annotation (user wording): **"Continue even if the tool cannot guarantee the
  integrity of the data."**
- `force` reads stay in place: `rt.config.process.force` everywhere. **No
  `let force = …` lifting / threading.**
- `force` stays hardcoded `true // TODO cli` (CLI rework pending). No new CLI
  flag. No serde-default / `Default` impl on `ProcessOptions` (no production
  data to migrate).

## Changes

### `config/process.rs`
`ProcessOptions` gains, next to `fail_fast`/`no_errors`:
```rust
/// Force: continue even if the tool cannot guarantee the integrity of the data.
pub force: bool,
```

### `config/extract.rs`
- Drop `pub force` from `ExtractConfig` (line 90 with the old wording).
- Drop `pub force_scan` from `ScanOptions` (69) and from its `Default` impl (79).
- `try_from` (~304), `for_place_test` (~380), `for_scan_test` (~452): remove the
  top-level `force: … ,` and the `scan: { …, force_scan: false, … }` bit; add
  `force: true, // TODO cli` into each `process: ProcessOptions { … }` block.
- `merge_over` (~547): drop top-level `force:`; add `force: self.process.force,`
  into the combined `ProcessOptions`.
- `DEFAULT_CONFIG` (~601 / ~639): drop top-level `force: false,` and the
  `force_scan: false,` scan bit; add `force: false,` into the DEFAULT
  `process:` block.
- `ScanOptions` keeps `#[serde(default)]`.

### `config/archive.rs`
Add `force: false,` to all three `ProcessOptions { … }` constructors
(~304, ~352, ~440). No archive-side consumer; behavior unchanged.

### `unarchive/scan.rs`
- 10 sites: `rt.config.scan.force_scan` → `rt.config.process.force`
  (159, 274, 290, 300, 370, 382, 405, 458, 461) and `rt.config.force` →
  `rt.config.process.force` (395).
- `report_scan_completeness(…, force_scan: bool)` → `force: bool`; call site
  passes `rt.config.process.force` (in-place, no lift).
- Message strings at 306 / 373 / 408: `--force-scan` → `--force`.

### `unarchive/rehash.rs`
- :123 `rt.config.force` → `rt.config.process.force`.
- Tests :969 / :981: `world.config.force = …` → `world.config.process.force = …`.

### `db/scan.rs`
- :49 doc comment reworded: `from_footer || force`.

## Verification

- `cargo build -p tar-dedup -p tar-dedup-cli`
- `cargo test -p tar-dedup --lib` (283 passing, 0 failures; rehash tests patched).