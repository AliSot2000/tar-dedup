# Plan: `slow-progress` debug feature

Status: agreed 2026-10-30. A cargo feature that makes every progress-bar increment
sleep briefly so bar behavior can be observed/debugged (`archive`, phase bars,
sub-bars, integrity checks, per-thread worker bars).

## Design — wrap the bar, not the call sites

`crate::progress` gains `pub struct ProgressBar(pub indicatif::ProgressBar)`:

- `Deref<Target = indicatif::ProgressBar>` — all non-increment methods (`set_length`,
  `set_message`, `finish_and_clear`, …) pass through unchanged.
- Inherent `inc(&self, delta: u64)`, `inc_len(..)`, `fetch_inc(..)`: **sleep first**
  (`#[cfg(feature = "slow-progress")]`), then delegate. That single choke point
  covers:
  - `ProgressBarSet::inc_both/inc_global/inc_phase` (they increment wrapped bars —
    the set's inc* functions therefore slow down transitively);
  - every bar the set hands out (`push_sub_bar`, `begin_integrity`, `thread_bar`,
    `hide_bar`, `PhaseRedirect`); the ~30 phase `.inc(..)` call sites stay untouched.
- Constructors `new`, `new_spinner`, `hidden` (mirror indicatif; `hidden()` is used
  in tests).

Sleep value: const `SLOW_PROGRESS_SLEEP = Duration::from_millis(100)` under the
feature; `slow_sleep()` is a no-op otherwise (feature off compiles to the same
code, zero runtime cost).

## Changes

- `crates/tar-dedup/Cargo.toml`: add `slow-progress = []` **not** wired into
  `test-all` (would make the suite crawl).
- `crates/tar-dedup/src/progress.rs`: newtype + `slow_sleep`; `ProgressBarSet`
  fields/returns switch to the newtype; internal `MultiProgress::add/insert/insert_before/remove`
  unwrap via `.0` (deref coercion handles the `&` anchor).
- Update the `use indicatif::ProgressBar` type annotations to
  `use crate::progress::ProgressBar` in: `archive/{inventory,hash,dedup,sparsify}.rs`,
  `archive_footer.rs`, `common/filter.rs`, `unarchive/{scan,rehash,place}.rs`
  (incl. `indicatif::ProgressBar::hidden()` → `ProgressBar::hidden()` in place tests).

## Verify

- `cargo check -p tar-dedup` (feature off — no behaviour change).
- `cargo check -p tar-dedup --features slow-progress`.
- `cargo test -p tar-dedup --lib` (off) stays green.
- Manual smoke: `cargo run -p tar-dedup-cli --features ...` — actually the feature
  lives on `tar-dedup`; build the CLI with `--features tar-dedup/slow-progress` and
  archive a small tree, observe the bars animate slowly, then exit cleanly (finish /
  abandon paths intact).
## Status — DEFERRED (2026-10-30)

Reverted (git checkout); the investigation stands for later resumption. Integration
tests take priority.

### Findings so far (verified)
- `ProgressBarSet` increments funnel through `inc_both` / `inc_global` / `inc_phase`
  (`progress.rs`), which delegate to wrapped `indicatif::ProgressBar`s; further bars
  are handed out via `push_sub_bar`, `begin_integrity`, `thread_bar` (`~30` phase
  call sites do `.inc(..)` straight on those handles).
- Solution: `crate::progress::ProgressBar` newtype (Deref to `indicatif::ProgressBar`,
  inherent `inc` that sleeps first under `#[cfg(feature = "slow-progress")]`). One
  choke point; no call-site edits. Constructors `new`/`new_spinner`/`hidden`
  mirrored (`hidden()` used in place.rs tests).
- indicatif pin is **0.17.11**: it has `inc`, `inc_length`, `fetch_modify` — there is
  **no** `fetch_inc`/`inc_len`, so the wrapper should wrap `inc` only (drop that
  version-dependent `inc_len`/`fetch_inc` idea).
- `MultiProgress::add/insert/insert_before/remove` need by-value `.0` unwraps for
  the forwarded bar (anchors coerce via Deref).
- Feature goes only in `crates/tar-dedup/Cargo.toml`, **not** part of `test-all`.
- Files needing the `use indicatif::ProgressBar` → `use crate::progress::ProgressBar`
  swap: `archive/{inventory,hash,dedup,sparsify}.rs`, `archive_footer.rs`,
  `common/filter.rs`, `unarchive/{scan,rehash,place}.rs`.
