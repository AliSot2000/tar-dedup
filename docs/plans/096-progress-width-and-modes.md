# 096 — Width-aware progress layouts and `--progress-mode`

Status: proposed (2026-11-16). Not implemented.

## Goal

Keep the existing progress information, but make every line fit the terminal width and survive resizes
without recreating bars. Introduce three layouts for `per-thread` mode and wire the existing
`--progress-mode {per-thread,global,none}` flag.

## Root cause

`ProgressBarSet` uses fixed templates such as:

```text
{spinner} {msg} [{bar:40}] {bytes}/{total_bytes} @ {bytes_per_sec}
```

Thread bars put the full absolute path into `{msg}`. Long paths exceed the terminal width, wrap, and
the wrapped rows desynchronize the multi-bar redraw. `set_phase_file` also caps at 56 chars but can
still overflow with a fixed 40-column bar. `ProcessOptions.progress_mode` exists but is never read.

## Decisions

- Normal mode keeps the current `Counter` / `Count` / `Bytes` / `BytesRate` templates and rates.
- Compact mode removes bar glyphs and shows `pos/len` on phase/sub bars.
- Percent mode (the requested "log" layout) keeps non-thread bars as percentage-only lines.
- Thread bars keep the `T` prefix in every layout where they are shown.
- Every bar stores its original untruncated message and re-renders from it on width/layout changes.
- Thread bars are never destroyed on narrowing; they are detached from display and re-attached later.
- Mode selection is fit-based, not hard width breakpoints.
- No `Phase:` / `Total:` prefixes are introduced. Existing messages stay (`hash`, `tar-dedup`, etc.).

## Layouts

### Normal

Current styles are unchanged except for message fitting and the thread prefix:

```text
⠙ T1: Hashing /home/as…xe/file.txt [========>---] 0 B/2.96 MiB @ 0 B/s
⠙ T2: Hashing /home/as…fe/file.txt [==========>-] 2.96 MiB/2.96 MiB @ 1.2 GiB/s
```

- `Counter`: `{spinner} {msg} {pos}`
- `Count`:   `{spinner} {msg} [{bar:40.cyan/blue}] {pos}/{len}`
- `Bytes`:   `{spinner} {msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} @ {bytes_per_sec}`
- `BytesRate` (phase): `{spinner} {msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} {prefix}`
- Global: `{msg} [{bar:48.green/black}] {percent:>3}%`
- Thread bars use a separate template with `{prefix}` for `Txx`:
  `{spinner} {prefix}: {msg} [{bar:40.cyan/blue}] {bytes}/{total_bytes} @ {bytes_per_sec}`

Phase/sub/global messages are not prefixed with `T`.

### Compact

No bar glyphs. One redrawn line per item:

```text
⠋ T1: /home/as…xe/file.txt  14%
⠋ T2: /home/as…fe/file.txt  96%
...
⠋ hash  1442/3014
⠋ DB Integrity  123/456
⠋ tar-dedup  16%
```

- Thread: `{spinner} {prefix}: {msg} {percent:>3}%`
- Phase: `{spinner} {msg} {pos}/{len}`; byte bars use humanized bytes.
- Sub-bars: spinner + existing message + `pos/len` (no percent).
- Global: `{spinner} tar-dedup {percent:>3}%`.
- Length-less `Counter` bars show `pos` only.
- Messages are fitted and padded so suffixes align.

### Percent (requested "log" layout)

Non-thread bars stay visible, percentage-only:

```text
DB Integrit...  11%
hash 100%
tar-dedup  30%
Scanned Entri...  1.12k
```

- No spinner, no bar glyphs, no counts (except length-less counters).
- Non-thread messages are tail-truncated and padded; suffixes are right-aligned.
- Length-less `Counter` bars show an SI-abbreviated count, not a percent.
- Thread bars are detached from `MultiProgress` but kept alive and updating internally.
- On width expansion the same thread handles are re-attached.
- This is still `MultiProgress` redrawing; no periodic `println!`.

## Thread prefix

- Thread indices are 1-based: `T1`, `T2`, ...
- Digit width is the decimal width of the worker count:
  `digits = max(1, floor(log10(count)) + 1)`.
- Examples:
  - 1 thread: `T1`
  - 8 threads: `T1` … `T8`
  - 10 threads: `T01` … `T10`
  - 16 threads: `T01` … `T16`
  - 128 threads: `T001` … `T128`
- No minimum of two digits.
- The prefix is used in Normal and Compact. Percent hides thread bars.

## Message fitting and padding

Every bar stores its original message (`raw`), plus its role/kind and fixed prefix.

Given a message budget `B` columns:

- If `measure_text_width(raw) > B`: middle-truncate to `<= B`, then pad the end to exactly `B`.
- If `measure_text_width(raw) <= B`: right-pad at the end to exactly `B`.
- The rendered message always occupies exactly `B` columns, so bar/suffix columns line up.

Middle truncation preserves the basename: `prefix…suffix`, then padding.

Percent mode tail-truncates instead: `prefix...`, then padding.

Display width is measured with `console::measure_text_width`, not `.chars().count()`, so `LÖSCHEN`,
CJK, and other wide characters fit correctly.

Fitting applies to **all** bars: global, phase, sub, and thread.

## Mode selection

Choose by fit. For every active bar role/kind, compute its required width as
`fixed_overhead(role, kind, prefix) + MIN_MSG`.

- `MIN_MSG_NORMAL = 12`
- `MIN_MSG_COMPACT = 8`

Selection:

1. Normal if `cols >= max(normal_required_width)` for all active bars.
2. Else Compact if `cols >= max(compact_required_width)` for all active bars.
3. Else Percent.

No "drop the rate first" middle step: Normal keeps rates; if Normal does not fit, Compact is chosen.
Every bar in a mode uses one consistent style.

`--progress-mode global` considers only the global bar, so it can stay Normal on narrower terminals.
`--progress-mode none` skips layout entirely.

Fixed-overhead values are conservative upper bounds derived from the kind and total:
- percent: 4 columns (`{percent:>3}%`)
- `pos/len`: `2 * digits(len) + 1`
- humanized bytes: width of the formatted total on both sides plus the slash
- rate: `" @ "` plus the maximum formatted rate width
- cumulative `BytesRate` prefix: the maximum `@ <rate> (<elapsed>)` width
- bar glyphs: fixed 40 columns in Normal; zero in Compact/Percent
- thread prefix: `T` + digit width + `": "`

## Resize behavior

All bars share one `LayoutState` with atomic width/mode. A dedicated
`tar-dedup-term-width` thread polls `console::Term::stdout().size_checked()` every ~250 ms while the
bar set is live.

On a width change:

1. Recompute the layout mode.
2. Re-fit every live bar from its stored original message.
3. Restyle non-thread bars for the chosen layout.
4. Detach thread bars from the `MultiProgress` when entering Percent.
5. Re-attach the same thread handles when leaving Percent.
6. Never recreate thread bars, never rewire workers, never destroy phase/sub/global handles.

The width watcher is stopped and joined from `ProgressBarSet::drop`.

`ProgressBarSet` exposes a test-only width override so layout transitions can be exercised without a
real terminal.

## Progress modes

- `per-thread` (default): global + phase + sub-bars + thread bars, using Normal/Compact/Percent.
- `global`: only the `tar-dedup` global line. Phase, thread, and sub bars are created but hidden and
  never inserted into the display. The global bar still advances normally.
- `none`: no bars at all, no percent lines, no thread bars. Normal logs continue to work.

Footer/integrity sub-bars follow the active layout and are suppressed under `global` / `none`.

## SI count formatting

Add `format_count_si(n: u64) -> String` in `common/formatting.rs`.

Rules:

- `< 1000`: integer (`100`)
- `>= 1000`: 1000-based SI suffixes
- Use up to 3 significant digits, trimming trailing zeros
- Promote at the boundary (`999.9M` -> `1G`)

Examples:

```text
100
1k
1.12k
999k
1M
999M
1G
1T
1P
1E
```

Assumption: lowercase `k` for kilo, uppercase `M/G/T/P/E`. The conversation examples mixed `1k` and
`1.12K`; this plan uses SI casing. Change to uppercase `K` if preferred.

Only length-less `Counter` bars in Percent mode use this formatter. Compact keeps raw `pos`.

## Suffix alignment

To keep columns stable after padding:

- Percent: right-align to `3 digits + %` (4 columns).
- SI count: right-align to a fixed 6 columns.
- `pos/len`: width derived from the total (digits or humanized bytes).
- The message budget is reduced by the chosen suffix width.

## Implementation outline

### `progress.rs`

- Add `console` as a direct dependency.
- Add `LayoutMode { Normal, Compact, Percent }`, `BarRole`, `LayoutState`.
- Add the width watcher and a live-bar registry used for refit/restyle.
- Extend the `ProgressBar` wrapper with:
  - shared original-message storage,
  - role/kind and fixed prefix,
  - `refit()` and style-sync helpers.
- Add `fit_middle`, `fit_tail`, `pad_to`.
- Add `ProgressBarSet::with_mode(multiplier, mode)`; keep `new()` as per-thread for tests.
- Thread prefix creation in `thread_bar()` using the pool size.
- Gate `begin_phase`, `push_sub_bar`, `begin_integrity`, `thread_bar`, `single_bar`,
  `create_thread_bars`, and `set_table_size` according to mode.

### Call sites

- Replace worker `set_message(format!("Hashing {}", path))` (and dedup/sparsify/rehash/place
  equivalents) with the fitted thread-message helper.
- Replace `set_phase_file`'s fixed 56-char cap with raw-message storage; the phase bar re-fits from
  that raw message on width changes.
- Pass `config.process.progress_mode` into `ProgressBarSet` in `archive::run` and `unarchive::run`.
- Keep `cmd/check.rs` on its own `--quiet` bar; it is outside `ProcessOptions`.

### Tests

- Fit/pad/pad-end tests at widths 20/40/80/120, ASCII, `LÖSCHEN`, CJK.
- SI formatter boundaries: 100, 1k, 1.12k, 999k, 1M, 999M, 1G.
- Mode selection: Normal/Compact/Percent at widths around each required boundary.
- `progress_mode=global`: only the global line is inserted.
- `progress_mode=none`: zero bars and zero percent lines.
- Render into `indicatif::InMemoryTerm` (enable its `in_memory` feature for tests):
  - every rendered line width <= terminal width at 40/80/120
  - compact has no thread raw numbers on thread lines
  - percent has thread lines absent
  - percent tail truncation and padding are correct.
- Resize simulation:
  - narrow -> wide restores previously hidden text and re-attaches the same thread handles
  - wide -> narrow truncates/pads the same handles
  - no new `ProgressBar` objects are created for threads.
- Thread prefix tests at 1/8/10/16/128 workers: `T1`, `T1..T8`, `T01..T10`, `T01..T16`,
  `T001..T128`.
- Percent mode length-less counter: `Scanned Entries 1.12k`.
- Existing `ProgressBarSet::new` tests remain per-thread.

### Docs

- This plan file.
- Update `--progress-mode` help and `docs/manual.md` with the exact semantics.
- Add a short progress-rendering note to `AGENTS.md`.

## Open items

- SI prefix case: plan assumes lowercase `k`, uppercase `M/G/T/P/E`.
- Compact length-less `Counter` bars: plan keeps raw `pos`.
- The user's `log10(...).min(1)` wording is interpreted as
  `digits = max(1, floor(log10(count)) + 1)` so a single thread is `T1` and 16 threads are `T01`.
