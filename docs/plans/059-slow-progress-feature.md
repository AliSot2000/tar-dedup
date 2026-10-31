# Plan: `slow-progress` debug feature

Status: agreed 2026-10-30 (implementing). A cargo feature that sleeps briefly before
every progress-bar increment so bar behavior can be observed/debugged (`archive`,
phase bars, sub-bars, integrity checks, per-thread worker bars).

## Design — one choke point, not call-site edits

`crate::progress` gains `pub struct ProgressBar(pub indicatif::ProgressBar)`:

- `Deref<Target = indicatif::ProgressBar>` — all non-increment methods
  (`set_length`, `set_message`, `finish_and_clear`, …) pass through unchanged.
- Inherent `inc(&self, delta: u64)`: `slow_sleep()` then delegate. That covers
  `ProgressBarSet::inc_both/inc_global/inc_phase` (they increment wrapped bars) and
  every bar the set hands out (`push_sub_bar`, `begin_integrity`, `thread_bar`,
  `hide_bar`, `PhaseRedirect`). The ~14 phase `.inc(..)` call sites stay untouched.
- Constructors `new`/`new_spinner`/`hidden` (mirror indicatif; `hidden()` is used in
  tests). Only `inc` is wrapped — the crate never calls `inc_len`/`fetch_inc`/
  `inc_length`/`fetch_modify` (verified), and `set_position` is not an increment.
- Partial wrap deliberately: everything else goes through `Deref`.

Sleep value: env `TAR_DEDUP_SLOW_PROGRESS_MS`, read **once** via `OnceLock<u64>`,
`std::env::var(..).ok().and_then(parse).unwrap_or(100)` → `Duration::from_millis`.
`slow_sleep()` is a no-op when the feature is off (zero runtime cost).

## Changes

- `../../crates/tar-dedup/Cargo.toml`: `slow-progress = []`, **not** wired into `test-all`.
- `../../crates/tar-dedup/src/progress.rs`: newtype + `slow_sleep`; `ProgressBarSet`
  fields/returns → newtype; internal `MultiProgress::add/insert/insert_before`
  unwrap by-value via `.0` (deref coercion handles the `&` anchors and `remove`).
- Swap `use indicatif::ProgressBar` → `use crate::progress::ProgressBar` in:
  `archive/{inventory,hash,dedup,sparsify}.rs`, `archive_footer.rs`,
  `common/filter.rs`, `unarchive/{scan,rehash,place}.rs`,
  incl. the two `indicatif::ProgressBar::hidden()` → `ProgressBar::hidden()` in
  place.rs tests.

## Verify (all on the VM)

1. `cargo check -p tar-dedup` (off) and `--features slow-progress` (on).
2. Full matrix built with `slow-progress` and run at `TAR_DEDUP_SLOW_PROGRESS_MS=1`
   (every increment flows through the sleep path, suite stays fast):
   `cargo test --workspace` · `cargo test -p tar-dedup --lib --features test-all` ·
   `cargo test -p tar-dedup --doc` · `bash integration/run.sh` ·
   `cargo test -p tar-dedup-cli --test integration_shell`.
3. Visual smoke at `TAR_DEDUP_SLOW_PROGRESS_MS=200`: CLI archive + extract a small
   tree (bars animate), clean finish; `--exit-after-stage inventory` for the
   `abandon` path.

## Notes
- `inc_both` intentionally double-sleeps (global + phase each increment) — inherent.
- Behavior-neutral when the feature is off; feature intentionally excluded from
  `test-all` so the suite does not crawl.
## Implementation notes (2026-10-30) — DONE

Implemented as planned: newtype `ProgressBar` (Deref + `inc` only) in
`crate::progress`, `slow_sleep()` reading `TAR_DEDUP_SLOW_PROGRESS_MS`
(`OnceLock<u64>`, `.unwrap_or(100)`), `ProgressBarSet` on the newtype
(`MultiProgress` by-value `.0` unwraps), the 9 files swapped to
`crate::progress::ProgressBar` (+ `hidden()` call sites), feature not in `test-all`.

### Verified — VM (Fedora 44)
- `cargo check` off and `--features slow-progress` on: ✓
- Feature-ON matrix at `TAR_DEDUP_SLOW_PROGRESS_MS=1`:
  lib `test-all,slow-progress` **717/0** · doctests 1/0 ·
  `integration_shell` (`--features tar-dedup/slow-progress`) **20/0** ·
  `bash integration/run.sh` **19 passed / 1 skipped** (btrfs symlink) ·
  release build ✓
- Baseline `cargo test --workspace` flipped 101 once on the *known flaky*
  `common::tests::on_sent_receives_only_handed_items` (passes isolated) — pre-existing,
  unrelated to this feature.
- Visual smoke at `TAR_DEDUP_SLOW_PROGRESS_MS=200`: 15-file archive ≈ 61 s (sleeps
  clearly active), extract OK, SIGINT mid-run → `Error: Interrupted` with the work
  dir kept. (An ad-hoc `resume` recipe flaked only because the tiny run finished
  before SIGINT — no work dir to resume; the deterministic resume path is covered
  by the `resume_archive` scenario, green under the sleeping build.)
