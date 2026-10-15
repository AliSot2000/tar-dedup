# Plan: shared MultiProgress bar set + log routing

## Goal
Replace the standalone per-phase progress bars (raw `indicatif::ProgressBar`,
`ByteProgress`, `CountProgress`) with one shared `MultiProgress` ("MPB") owned by
a `ProgressBarSet` threaded through `ArchiveRTArgs` / `ExtractRTArgs`. The MPB
shows:
- a fixed **bottom global bar** (overall pipeline progress; no counter/eta/bytes),
- above it the **current phase bar(s)** (may carry counter/eta/bytes),
- for complex phases (place, place_prologue) extra **sub-bars**.

Log output is split: `tracing::error` → **stderr**; warn/info/debug → **stdout**
printed above the bars through the MPB so nothing is clobbered.

## Locked decisions (from discussion)
- Bars draw on **stdout** (indicatif default); errors on a plain **stderr** layer.
- Extract multiplier = **6** (Cleanup does no per-element work and is excluded).
  Archive multiplier = **7**.
- **Swap every existing progress bar onto the MPB in this pass**; per-phase
  polish (real totals, sub-bars) lands incrementally afterwards.
- Global `inc` increments the phase bar AND the global. (`inc_both`.)
- **Scheme B** for subset phases (dedup, hash, sparsify, tar_writer): the phase
  bar is sized to the *remaining candidates* after bulk skips; bulk
  `promote_*` results feed the global only (`inc_global`), loop items feed both
  (`inc_both`).
- Serial "shove-along" promotions (dedup.rs:177–180 style) count against the
  global the moment they run.
- Log default level hard-coded **INFO** (RUST_LOG still overrides). **No new
  CLI verbosity flags this pass** — the CLI log args are deferred (user has a
  design in mind).
- Global bar semantics: **Case +50** (see increment contract below).
- Thread-level progress: **aggregate per-phase bar now**; per-thread bars
  (msg = file, inc = bytes) are designed but gated behind a future config flag.

## Bar layout & increment contract

### Layout (top → bottom)
```
[sub-bar 1]            <- only for complex phases (place / place_prologue)
[sub-bar 2]
[phase bar]            <- current phase; Counter / Count / Bytes
[global bar]           <- bottom; "{msg} [{bar:48}] {percent:>3}%" — NO pos/len/eta/bytes
```
`push_sub_bar` inserts above the phase bar. At `begin_phase` the previous
phase/sub bars are `finish_and_clear` + `remove`d and a fresh phase bar is
inserted just above the global.

### Global semantics (Case +50)
- `set_table_size(n)` → `global.set_length(n * multiplier)`; keeps `table_size`.
- `begin_phase(idx)` snaps `global.set_position(idx * table_size)` — both the
  "finish off the previous phase" safety net and the resume anchor.
- Each file advances the global **exactly once per phase, the moment it leaves
  the phase** (via promotion or per-row processing). Promotions therefore count
  immediately, no remap, no end-of-phase credit.

### workded example (dedup, table = 100, offset = 3×T)
| moment | action | phase bar | global |
|---|---|---|---|
| begin | 4× `promote_*` (40 ineligible) → `inc_global(40)`; `set_phase_total(60)` | 0/60 | offset + 40 |
| 10 eligible done | `inc_both(10)` per row | 10/60 | offset + 50 |
| 50 pending | `inc_both(1)` each | …/60 | monotonic |
| end | loop drains | 60/60 | offset + 100 |

"Already done" rows shown by the phase bar read `10/60`; the global reads
`offset + 50` because 50 files have left the phase (40 SQL + 10 compare).
This is **Case +50**: skips → `inc_global`, loop → `inc_both`. The phase bar
(% of remaining candidates) and the global (% of files completed in the phase)
are intentionally different numbers.

## New `progress.rs` API
```rust
pub enum BarKind { Counter, Count, Bytes }
// Counter:  "{spinner} {msg} {pos} items"
// Count:    "{spinner} {msg} [{bar:40}] {pos}/{len}"
// Bytes:    "{spinner} {msg} [{bar:40}] {bytes}/{total_bytes} {bytes_per_sec}"

pub struct ProgressBarSet {
    mp: MultiProgress,            // draw target stdout
    global: ProgressBar,          // bottom, no counter/eta/bytes
    current: Vec<ProgressBar>,    // [sub_bars…, phase bar] above global
    table_size: u64,
    multiplier: u64,
}

impl ProgressBarSet {
    pub fn new(multiplier: u64) -> Self;
    pub fn set_table_size(&self, n: u64);                 // global len = n * multiplier
    pub fn begin_phase(&self, idx: usize, msg: &str, kind: BarKind);
    pub fn set_phase_total(&self, n: u64);
    pub fn set_phase_msg(&self, s: &str);
    pub fn set_phase_kind(&self, k: BarKind);             // spinner -> bar switch (scan / place_prologue)
    pub fn push_sub_bar(&self, msg: &str, kind: BarKind); // insert above phase bar
    pub fn inc_both(&self, n: u64);                       // phase bar AND global
    pub fn inc_global(&self, n: u64);                     // global only (bulk promote_*)
    pub fn inc_phase(&self, n: u64);                      // phase bar only (rare)
    pub fn finish(&self);                                 // complete global
    pub fn abandon(&self);                                // Interrupted / exit-after-stage
}
```
- Phase ordinals: `PipelinePhase::index()` (Inventory 0 … Archive 6),
  `ExtractPipelinePhase::index()` (ScanTar 0 … Permissions 5, Cleanup 6).
- Constants: `ARCHIVE_MULTIPLIER = 7`, `EXTRACT_MULTIPLIER = 6`.
- Extract `Cleanup`: no bar, just snap global to `6 × table_size`.
- MultiProgress + bars are `Sync`; worker threads (`compare_one`, `walk`, …)
  receive a borrowed `&ProgressBar` / `&ProgressBarSet`.
- Delete `ByteProgress` / `CountProgress` or reduce them to internal style
  builders; hash.rs TODO "use our wrapper" goes away.

### bar helper (follow-up, not blocking this pass)
Per-thread bars for dedup/hash/sparsify: each rayon thread creates a `Bytes` bar
(msg = file, inc = bytes) above the phase bar; off by default, guarded by a
future config flag (`--per-thread-progress`).

## Log routing
- `progress::init_tracing()` in the lib replaces the subscriber init in
  `crates/tar-dedup-cli/src/main.rs`:
  - Layer A (ERROR only): `fmt::layer().with_writer(std::io::stderr)`,
    filter = `EnvFilter` ∧ level == error.
  - Layer B (WARN..TRACE): custom `MakeWriter`:
    - MPB registered and stdout is a tty → `MultiProgress::println(line)`,
      which renders the line above all bars as one coordinated frame
      (multi-line / wrapped events cannot desync the cursor). ANSI is off on
      this layer so the lines count correctly.
    - else (hidden/not registered) → plain stdout.
    filter = `EnvFilter` ∧ level < error (no ERROR duplication).
    Events are pushed to a queue drained by a dedicated thread (~25 Hz) that
    joins bursts (e.g. the hash WARN storm) into a single `println` frame, so
    the output stays Docker-like instead of one redraw per event. Known
    trade-off: `MultiProgress::println` accumulates printed lines in the frame
    for the run's lifetime (unbounded for extremely chatty runs).
  - Base filter: `EnvFilter::builder().with_default_directive(LevelFilter::INFO.into()).from_env_lossy()`.
- Global `static LOG_MPB: Mutex<Option<Arc<MultiProgress>>>`; `ProgressBarSet`
  stores an `Arc<MultiProgress>`; `run` registers on start and clears on exit.
- Existing `eprintln!` sites in `archive::run` / `unarchive::run` stay as-is.

> **Why not `suspend`**: an external write under `suspend` must keep the cursor
> inside the erased bar area; a tracing event may span more terminal lines
> (timestamp + multi-line message) than the area, so the following redraw
> re-emits bar lines into the log region. `println` owns the full frame
> (`[log lines, …bar lines]`) inside indicatif's own accounting.

## Wiring
- `ArchiveRTArgs` and `ExtractRTArgs` gain `pub progress: &'a ProgressBarSet`.
- `archive::run`: create set (×7) after acquiring the workdir lock; register for
  the logger; per loop iteration `begin_phase(state.phase.index(), name, kind)`;
  after inventory completes — or on resume via `db.count_entries()` — call
  `set_table_size(n)`; Interrupted / `--exit-after-stage` → `abandon()`;
  completion → `finish()`; unregister on exit.
- `unarchive::run`: create set (×6) up front; register; ScanTar gets a Counter
  (unknown member total); once scan returns, `set_table_size(count_entries)`;
  per extract phase `begin_phase`; Cleanup snaps global to `6 × table_size` and
  skips a bar; abandon/finish/unregister as above.

## Phase migration (this pass)
Every existing bar is swapped onto the MPB, preserving current behavior, then
polished where trivial.
| Phase | Current | → new |
|---|---|---|
| archive/inventory | `CountProgress::new` | Counter (inc / entry) |
| archive/hash | bare `ProgressBar::new(total)` | Count + `set_phase_total` |
| archive/filter | none | Counter |
| archive/dedup | `with_total(candidates)` | Scheme B: `inc_global` for the 4 `promote_*` skips (dedup.rs:177–180), `set_phase_total(candidates)`, loop `inc_both` |
| archive/sparsify | `with_total` | Count (candidates) |
| archive/stage | none | Counter (+ promote counts) |
| archive/tar_builder | `ByteProgress` | Bytes, real byte total |
| unarchive/scan | none | Counter (tar members) |
| unarchive/filter | none | Counter (file rows) |
| unarchive/rehash | bare `ProgressBar::new` | Count (payload candidates) |
| unarchive/place_prologue | none | Counter → `set_phase_total(count_out_tree_rows())` once out_tree built |
| unarchive/place | none | Count(out_tree rows) phase bar + `push_sub_bar` per op (copy / hardlink / others) |
| unarchive/permissions | none | Counter |

## DB helpers (follow-up pass, spec'd now)
- `count_archive_payload_candidates()` = canonical payloads
  `canonical_id = id AND AppendedPath AND NOT ErrorWhileArchive` — extract
  rehash total.
- Extract flow: force-scan spinner → bar once the DB appears; filter/rehash
  process file rows; place_prologue spinner → Count rescaled to
  `count_out_tree_rows()` (exists, db.rs:724); place = lower bar over all
  out_tree rows, upper bar per eligible op.
- Audit: every phase that passes the whole table needs the bulk `promote_*`
  + matching `inc_global` (hash is a known "missing" case).

## Edge cases
- Non-tty: MPB auto-hidden; `suspend`-based stdout logs still emit; no banner.
- Resume mid-pipeline: `begin_phase` anchors from a DB row count read up front.
- Interrupted / `--exit-after-stage`: `abandon()` + unregister logger — no
  dangling bar on the terminal.
- `suspend` serializes writes against rayon increments (short ops only).

## Verification
- `cargo check -p tar-dedup`, `cargo check --features err-to-panic -p tar-dedup`,
  `cargo check -p tar-dedup-cli`; `cargo test -p tar-dedup --lib`
  (expect the same 4 pre-existing failures).
- TTY smoke (small tree, archive + extract):
  - global bottom bar has no counter/eta/bytes;
  - phase bar sits above it with its own counter/eta;
  - `2>` shows only `tracing::error`; info/warn lines appear above the bars;
  - dedup phase bar shows remaining candidates (Scheme B) and the global follows
    the +50 contract;
  - RUST_LOG unset ⇒ INFO default; interrupting leaves a clean terminal.
- Piped run: stdout carries log lines only, no banner.

## Follow-ups (separate passes)
1. Extract payload-candidate counts + place multi-sub-bar polish.
2. Per-thread sub-bars behind a future config flag (aggregate stays default).
3. CLI verbosity flags (user's own design; deferred).
4. **Feature idea**: colored stdout logs. ANSI is currently disabled on the
   stdout layer because colored lines would corrupt indicatif's width/line
   accounting when routed through `MultiProgress::println`. Re-enable via a
   safe wrapper (strip ANSI for width computation but re-emit for display, or
   an upstream indicatif feature). Errors keep color on stderr.