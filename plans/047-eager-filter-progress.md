# Plan: eager-filter progress bars

Status: **implemented (2026-10-20).** When the extract filter runs **eagerly during the scan
phase** (`scan.rs::store_filter_buffer` → `filter::run(rt, true)`), it currently drives
the *scan* phase bar via the shared `joint_filter_phase`/`step_filters` (which call
`pg.set_phase_total` / `pg.inc_both` / `pg.inc_global`), corrupting the scan bar's
length and double-counting rows on the global. The standalone `Filter` phase (which
runs after scan) already has proper phase-bar + global-block + loading-bar progress.

Goal: give the eager filter its own progress display — a **filter** sub-bar plus the
existing **Loading Filters** sub-bar — stacked above the scan phase bar, without
touching the scan bar or the global. The global block is supplied by the *second*
filter call (the standalone `Filter` phase), so the eager call must not advance the
global (avoid double-counting rows already in the scan slice).

## Design: scoped phase-redirect + stack-ordered sub-bars

`joint_filter_phase`/`step_filters` are shared by archive + extract and stay **unchanged**:
they keep calling `pg.set_phase_total`/`inc_both`/`inc_global`/`push_sub_bar`. A
temporary override on `ProgressBarSet` redirects those calls to a dedicated sub-bar
during the eager filter, then restores.

### Sub-bar ordering (stack semantics)
`push_sub_bar` currently does `mp.insert_before(phase, bar)` — each new sub-bar is
wedged between older sub-bars and the phase bar (newest lowest). Change it to
`mp.insert(0, bar)` so new sub-bars are **appended to the top of the stack** (newest on
top), which also lets the method drop its `phase`-lock + `Some/None` fallback entirely.
Only affects the eager-filter multi-bar stack (all other `push_sub_bar` users have a
single sub-bar, so their layout is unchanged; `thread_bar`/`begin_phase` untouched).

Eager-filter stack, top → bottom:
```
[Loading Filters…]   <- step_filters sub-bar; hidden when trivial; hidden after parse
[filter]             <- redirected progress bar; removed by the guard on drop
[extracting]         <- per-file byte bar (dormant during filter; untouched)
[scan phase bar]     <- untouched (position/length preserved)
[global]             <- untouched (no double-count)
```

## Changes

### `progress.rs`
1. `push_sub_bar`: `self.mp.insert(0, bar.clone())` instead of the phase-lock + match.
2. New field `override_phase: Mutex<Option<ProgressBar>>`.
3. Route phase-targeting methods through the override when set (override wins over the
   real phase bar): `set_phase_total`, `set_phase_position`, `set_phase_msg`,
   `set_phase_kind`, `inc_phase`. `inc_both`/`inc_global` advance the override bar
   **only** (global decoupled) while it is active; otherwise behave as today.
4. `redirect_phase(bar) -> PhaseRedirect`: stores the previous override, restores it on
   `Drop`, and finishes + removes `bar` (and drops it from `subs` by ptr identity).
5. `hide_bar(&ProgressBar)`: `finish_and_clear` + `mp.remove` + `subs` retain-by-ptr.
6. `phase_position()` / `global_position()` getters (needed for the redirect unit test;
   also closes the earlier "progress-seed" observation gap).
7. Unit test: `redirect_phase` routes `inc_both`/`inc_global`/`inc_phase` to the
   override bar, leaves global + real phase untouched, and restores on drop.

### `common/filter.rs`
- `joint_filter_phase` fast path fixed: replaced `inc_global(updated)` + `inc_both(total)`
  (which double-counted) with a single `pg.inc_both(updated)`, and moved the
  `pg.inc_both(already_filtered)` seed into the **real-filtering** branch so the fast
  path goes 0 → `total` in one advance. The `debug_assert_eq!(total, updated)` stays:
  `apply_no_filter` updates **every** row (no `WHERE`), so `updated == total`; the
  `already_filtered` seed counts rows in the source phase `pv` (== `total` on a fresh
  run), so a `total - already_filtered` assertion would fail (`0 == total`).
  (Note: the non-fast-path seeding of `already_filtered` (source-phase count) and
  `step_filters`' per-row double `inc_both` remain pre-existing accounting quirks,
  unchanged by this plan.)
- `step_filters`: compute `filter_total` once; only push the "Loading Filters from the
  DB…" sub-bar when `filter_total > 1` (hidden when trivial; the no-filter fast path
  never reaches `step_filters` anyway). After both `parse_filter` passes,
  `pg.hide_bar(&add_flt)` so it disappears once loading completes.

### `scan.rs` `store_filter_buffer` (eager branch)
- If `eager_filter`: push a `filter` sub-bar (`BarKind::Counter`) and wrap
  `filter_run(rt, true)` in a `redirect_phase` guard (kept alive across the call via
  `let _guard`). The guard removes the bar on drop.
- **Always** redirect when the eager filter runs — including the trivial fast path,
  which still advances progress (its `inc_both` on the row count), so a
  trivial/non-trivial split would leak those advances onto the scan bar/global. No
  `get_extract_filters_applied()` gate here: `filter_run` already no-ops internally
  when the filters were applied (resume), in which case the bar never advances and is
  simply removed — the outer check was redundant (adding/removing a sub-bar is cheap,
  and the real gate is `filter_run`'s, not a perf guard).

### Tests (`scan.rs`)
- Extend `scan_eager_filter_skips_excluded_payload_member`: assert
  `db.get_extract_filters_applied() == Some(true)` after the eager run.
- Add `scan_eager_no_user_filters_takes_fast_path`: `eager_filter` on with no user patterns →
  run succeeds, all payloads extracted, `ExtractFiltersApplied` set (fast path).

## Not changing
- The `ExtractFiltersApplied` meta key / resume-skip (`unarchive/filter.rs` already
  guards `joint_filter_phase` on it).
- Standalone `Filter` phase progress (already correct).
- `thread_bar` / `begin_phase` ordering.
