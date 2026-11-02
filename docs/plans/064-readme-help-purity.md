# 064 — Readme option tables mirror `--help`

Status: approved in conversation (build).

## Goal

The `Readme.md` is the manual (man-page level). Its option tables must be
**pure**: every Description cell equals the argument's clap `--help` text
(the doc comment), one `help_heading` group per table, section headers named
exactly after the `help_heading` values. Additional "how it's done" material
lives only in the Notes column and the section-intro prose — which are **not**
touched by the rewrite.

## Principle decisions (settled)

- Description cell = `arg.get_help()` verbatim; multi-line doc comments are
  joined with spaces onto one cell line.
- Section headers = `help_heading` values (`Sidecar Manifests` → `Sidecar`,
  default-heading subcommands (`resume`/`reset`) → `Options`).
- **Strikethrough for diverged rows**: rows that are gone/merged from the CLI
  are kept (optionally) struck through in **all six columns**
  (`~~--option~~` …) so old descriptions stay copyable when a new, merged, or
  augmented option reuses them. The generator has a keep/drop switch.
- **Scripted purity check**: a script compares the Readme's non-struck rows
  against the compiled clap metadata and fails on drift, stale rows, or a
  missing arg. Struck rows are exempt.

## 1. Source-of-truth fixes (`cli.rs`, no behavior change)

- extract `--db-retention` help still reads like the old `--keep-db`
  ("keep a timestamped copy of snapshot.sqlite next to the archive"); align
  with the archive wording (`when - always/on-error/never`, and note the kept
  DB lands under the extraction root).
- `--absolute` doc: drop the stray `,/` and restore the missing closing `)`.
- Value-enum variant docstrings already exist (`MapResolutionTarget`,
  `ConflictPolicy`, `HardLinkGrouping`, `DedupMode`, `ArchiveOrdering`,
  `ProgressMode`, `DbRetention`, `SidecarMode`), so clap prints "Possible
  values" with docs; only spot-check flag docstrings that enumerate values.

## 2. Generator (source of truth is clap)

- New `tar-dedup-lib` example `examples/gen_help_tables.rs`: walks
  `Cli::command()` / each subcommand, emits JSON rows
  `{ subcommand, heading, short, long, required, args, description }`
  from `get_help_heading()/get_short()/get_long()/is_required()/get_num_args()/get_help()`.
- `scripts/gen_readme.py`:
  - parses the current Readme, replaces only the *table block* under each
    `**Heading**` (heading + intro paragraphs untouched);
  - maps `None` heading → `Options`;
  - inserts the **obsolete registry** (below) as struck rows after the
    affected tables (or drops them with `--drop-obsolete`);
  - `--check` mode: assert every non-struck produced row exists in the Readme
    and vice versa, and Description/Short/Required/Args match the clap output;
    exit non-zero on any drift.

## 3. Obsolete registry (struck rows, all columns)

Archive only — removed flags with a superseded-by note:

| Struck row | Note |
|---|---|
| `--auto-compress` / `--no-auto-compress` | suffix inference is the default now |
| `-P --absolute-names` (Indexing) | archive has no such flag; extract has `--absolute` |
| `--no-anchored` / `--no-ignore-case` | unanchored / case-sensitive are the defaults |
| `--eager-filter` | the default; only `--lazy-filter` exists |
| `--retry-missing-sha` | removed |

Deliberately NOT struck: rows whose flag still exists but whose text drifted —
those are corrected to the help text (purity).

## 4. Retention of untouched content

All section-intro prose (preconditions, "no writes to the indexed files",
filtering deviations, …), the numbered **Notes** block, `Motivation`,
`Disclaimers`, docs about the archive format, and the `### resume/inspect/…`
intros stay byte-for-byte unless the table itself is replaced.

## Implementation order

1. `cli.rs` doc fixes (both subcommands' help stays consistent).
2. `examples/gen_help_tables.rs` + run → inspect JSON (heading/args mapping).
3. `scripts/gen_readme.py` (splice + registry + `--check`), run, review diff.
4. Purity check green; `cargo build` / `cargo test -p tar-dedup-lib --lib`
   still green; spot-check `tar-dedup archive --help` vs a table.
## Implementation notes (2026-11-02)

- `cli.rs` fixed: extract `--db-retention` help aligned with the archive wording
  (note: kept DB lands under the extraction root); `--absolute` doc got its
  missing closing paren (stray `,/` was never in the source).
- `crates/tar-dedup-lib/examples/gen_help_tables.rs`: emits JSON rows per
  subcommand + `help_heading` from clap metadata (`description` = doc comment
  joined onto one line; `short/long/required/args` from clap). `get_num_args()`
  is `None` for inferred args, so `args` falls back to the action (flag → `0`,
  single → `1`, `Append` → `1+`). The global `--log-level` is attributed to the
  archive subcommand only (matches the Readme layout).
- `scripts/gen_readme.py`:
  - replaces only the table region under each `**Heading**` (heading name = the
    `help_heading`; `Sidecar Manifests` → `Sidecar`), keeping intro paragraphs.
  - carries Notes over by option key so existing editorial notes are untouched.
  - renders the OBSOLETE registry struck through in **all six columns**, or
    omits it with `--drop-obsolete`. Idempotent (re-running is a no-op).
  - `--check` scans non-struck rows and fails on drift, stale rows, or missing
    args; struck rows are exempt. Ran clean.
- Obsolete struck rows: `--auto-compress`/`--no-auto-compress`,
  archive `-P --absolute-names`, `--no-anchored`, `--no-ignore-case`,
  `--eager-filter`, `--retry-missing-sha`.
- Verified: `cargo build` clean, 716/0 lib tests, every table row has exactly 7
  cells, and a sample `extract --help` dump matches the corresponding table.
