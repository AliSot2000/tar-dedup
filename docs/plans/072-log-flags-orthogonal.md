# 072 — Orthogonal logging flags (`--log-format` preset + per-axis overrides)

Status: **implemented (2026-11-06).**

## Problem

`--log-format short|full` conflated two independent axes into one `full` bool in
`install_tracer`: **(a)** whether the module `target` is shown, and **(b)** the
default time precision (seconds vs sub-second). `--log-time` only overrides time,
so the preset + template were not orthogonal — you could not pick an arbitrary
(target, precision) without expressing the time yourself.

## Design — mirror the `--apply-all-metadata` / `--no-*` resolution

`resolve_bool_flag(no, yes, base)` (`config/common.rs:17`) gives: `--no` wins →
`--yes` → else `base`. Apply that per axis.

### CLI (global `Logging` heading on top-level `Cli`)
- `--log-format <short|verbose>` — the global preset (rename `full` → `verbose`):
  - `short` → base `(seconds, hide target)`
  - `verbose` → base `(sub-second, show target)`
- `--log-short-time` / `--log-long-time` — override the **time** axis.
- `--log-no-target` / `--log-target` — override the **target** axis.
- `--log-time <STRFTIME>` stays a **separate** string option; when present it
  overrides the entire short/long time axis (higher precedence).

### Resolution (`Cli::resolve_log_flags() -> (long_time, target_visible)`)
```rust
long_time      = resolve_bool_flag(log_short_time, log_long_time, format == Verbose)
target_visible = resolve_bool_flag(log_no_target,  log_target,    format == Verbose)
```

### `progress.rs`
`init_tracing(log_level, long_time: bool, target_visible: bool, log_time: Option<&str>)`;
`install_tracer` param `full` renamed to `target_visible` (only used for
`with_target`). Timer: `StrfTimer(t)` if `--log-time`, else `SecondTimer`
(`long_time == false`), else `SystemTime`.

### `main.rs`
Call `cli.resolve_log_flags()` then `init_tracing(cli.log_level, long_time, target_visible, cli.log_time.as_deref())`.

## Resulting matrix (every corner reachable independently)
- default → `…sZ WARN message`
- `--log-format verbose` → sub-second + target
- `--log-format verbose --log-short-time` → seconds + target
- `--log-format short --log-target` → seconds + target
- `--log-time "%H:%M:%S"` → template (short/long axis ignored)

## Why `--log-time` stays a separate string flag
`clap::ValueEnum` can only name a fixed set of variants; an arbitrary strftime
template is open-ended content and cannot be a variant. The short/long axis is a
boolean toggle (folds into `resolve_bool_flag`); the template is a distinct
higher-precedence override, so `Option<String>` is the correct shape.

## Files touched
- `crates/tar-dedup-lib/src/cli.rs`
- `crates/tar-dedup-lib/src/progress.rs`
- `crates/tar-dedup/src/main.rs`
- `docs/manual.md` (regenerate)

## Tests
- `cli.rs`: update `LogFormat::Full` → `LogFormat::Verbose`; parse the 4 new
  toggles; a `resolve_log_flags()` table covering all overrides; invalid
  `--log-format` value rejected.
- `progress.rs`: existing `SecondTimer`/`StrfTimer`/`level_filter` tests unchanged.
- Build + full `--features` lib suite (VM `test-all` when the VM is up).