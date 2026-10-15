# Plan: configurable regex Unicode mode (`FilterOptions.no_unicode`)

Status: **agreed (2026-10-12), implementation round.** User chose the new
`FilterOptions` field over folding into `relax_utf8`.

## Goal

Replace the hardcoded `const REGEX_UTF_8: bool = true` in `common/filter.rs` with a
config knob, resolving both `// TODO needs to be done with --force-utf8` comments.
New option: `no_unicode: bool` on `FilterOptions` (default `false` = Unicode ON,
today's behavior), CLI `--no-unicode` under the **Filtering** heading on both
`archive` and `extract`. `--no-unicode` compiles the filter regexes in bytes mode
(`.` matches a byte, `\w`/`\d`/`\s`/case-folding are ASCII-only).

Rationale: orthogonal to `relax_utf8` (a path *data* policy on skip-vs-abort); the
regex mode is a *matcher* knob in the same family as `anchored`/`ignore_case`, which
already live in `FilterOptions` and already thread through `parse_filter` /
`handle_query` as params.

## Changes

### `config/filter.rs`
- Add `pub no_unicode: bool` to `FilterOptions`.
- Add `no_unicode: false` to `Default`.

### `common/filter.rs`
- Delete `const REGEX_UTF_8: bool = true;` and both `// TODO needs to be done with --force-utf8`.
- `parse_filter` — add `no_unicode: bool` param (next to `anchored`/`ignore_case`),
  `.unicode(!no_unicode)`.
- `handle_query` — add `no_unicode: bool`, `.unicode(!no_unicode)`.
- `handle_pattern_source` and `ingest_filters` — thread `no_unicode` (they already
  carry `ignore_case`).
- `joint_filter_phase` / `step_filters` — add `no_unicode` param, pass into `parse_filter`.

### `cli.rs`
- `ArchiveArgs` (next to `anchored`/`ignore_case` at :219-228): `--no-unicode`,
  SetTrue, default false, `help_heading = "Filtering"`.
- `ExtractArgs` (next to `ignore_case` at :1016): same.

### `config/archive.rs` / `config/extract.rs`
- Add `no_unicode: args.no_unicode` to the `FilterOptions` literals
  (`archive.rs:423`, `extract.rs:497`).

### Callers
- `archive/filter.rs`: `joint_filter_phase(... rt.config.filter.no_unicode ...)`;
  `ingest_filters` -> `ingest_filter_rules(... config.filter.no_unicode ...)`.
- `unarchive/filter.rs`: `joint_filter_phase(... rt.config.filter.no_unicode ...)`;
  `ingest_filters` -> `internal_ingest_filter(... config.filter.no_unicode ...)`.
- Archive test-world builders use `FilterOptions { eager_filter: false, ..Default }`
  spreads -> no change needed.

## Verification

- `cargo build -p tar-dedup -p tar-dedup-cli`
- `cargo test -p tar-dedup --lib`
- `--help` shows `--no-unicode` under `Filtering:` on both subcommands.
