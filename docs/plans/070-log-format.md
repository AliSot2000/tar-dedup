# 070 — Configurable tracing output (`--log-format` / `--log-time`)

Status: **agreed (2026-11-06), implementation.** Adds runtime control of the
`tracing` line format: a two-preset `--log-format` flag plus a free-form
`--log-time` strftime over-ride, and wires the previously dead `--log-level`.

## Problem

`progress.rs::init_tracing()` (called at `main.rs:11`, before arg parse) built
two `fmt::layer()`s with the default formatter — microseconds RFC3339 timestamp
and the full `target` (e.g. `tar_dedup_lib::common::files`). Neither was
configurable, and `--log-level` (`cli.rs:21`) was defined + documented but never
applied (init runs before parse and ignores it).

## Design

### `cli.rs` — three global Logging flags on top-level `Cli`
- `--log-level` (existing `LogLevel` enum) — now actually wired.
- `--log-format <short|full>` (`LogFormat`) — `Short` (default) = seconds
  timestamp + no target; `Full` = default tracing (sub-second + target).
- `--log-time <STRFTIME>` (`Option<String>`) — free chrono strftime that
  over-rides the timestamp rendering of whichever preset.

All `global = true`, `help_heading = "Logging"`.

### `main.rs`
`init_tracing()` moves **after** `Cli::parse()` (so it can see the flags):
```rust
let cli = Cli::parse();
let shutdown = Shutdown::install()?;
tar_dedup_lib::progress::init_tracing(cli.log_level, cli.log_format, cli.log_time.as_deref());
match cli.command { /* ... */ }
```

### `progress.rs`
New signature `init_tracing(log_level: Option<LogLevel>, format: LogFormat,
log_time: Option<&str>)`:
- EnvFilter default directive from `--log-level` (`LevelFilter::INFO` when
  unset); an explicit `RUST_LOG` still wins (`from_env_lossy` semantics).
- `.with_target(full)` on **both** layers (error→stderr, below-error→stdout) so
  lines stay consistent.
- Timestamp precedence (applied to both layers):
  1. `--log-time <t>` → `StrfTimer(t)` → `chrono::Utc::now().format(t)`;
  2. else `Full` → default tracing timer (sub-second);
  3. else `Short` → `SecondTimer` → `%Y-%m-%dT%H:%M:%SZ`.
- Two `FormatTime` impls: `SecondTimer` (fixed UTC seconds + `Z`) and
  `StrfTimer(String)` (owned template). Plus a `LogLevel → LevelFilter` map.

chrono renders unknown strftime specifiers literally/best-effort (no panic), so
no up-front template validation is needed.

## Examples
- default / `--log-format short` → `2026-11-06T01:12:28Z WARN …`
- `--log-format full` → today's verbose output
- `--log-format full --log-time "%H:%M:%S"` → `13:37:42 WARN tar_…::common::files …`
- `--log-format short --log-time "%a %d %b %T"` → `Fri 06 Nov 13:37:42 WARN …`

## Files touched
- `crates/tar-dedup-lib/src/cli.rs`
- `crates/tar-dedup-lib/src/progress.rs`
- `crates/tar-dedup/src/main.rs`
- `docs/manual.md`

## Tests
- `cli.rs`: parse `--log-format short|full`, `--log-time` passthrough, invalid
  enum value rejected.
- `progress.rs`: `SecondTimer` matches `^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$`;
  `StrfTimer` honors the template; `LogLevel→LevelFilter` map. No live-subscriber
  test (`registry().init()` cannot run twice in one binary).