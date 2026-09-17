# Plan: scaffolding for the catalog / reset subcommands

## Problem
Five new subcommands are needed eventually: `inspect`, `list`, `dump`, `query`
and `reset`. This pass is **scaffolding only**: register the commands, wire the
`-f`/`--db`/`--work-dir` input plumbing, and leave each `run()` returning an
`Error::Config(… not implemented)` until the real behavior is designed.

## Decisions
- Read commands (`inspect`/`list`/`dump`/`query`): the catalog comes from the
  archive footer via `-f ARCHIVE`, or from an existing work DB via `--db FILE`
  xor `--work-dir DIR` (mirrors `resume`). Priority: `--db` > `--work-dir` > `-f`;
  at least one required. Relative paths resolve against the current directory.
  Explicitly **no `-C`/`--directory`** on these commands.
- `reset`: `--db`/`--work-dir` **required**, no `-f` (exactly like `resume`).
- Every command currently reports itself unimplemented:
  `Error::Config("`<name>` is not implemented yet")`, after its input has been
  resolved and validated (so the `-f`/`--db` plumbing is real). The actual
  implementations (inspect summary, column-selected list, csv/json dump, table
  search, parameter-merge + phase rewind) are follow-up work.

## Surface
- `crates/tar-dedup/src/cli.rs`: `Command` gains `Inspect`/`List`/`Dump`/`Query`/
  `Reset`; new `ArchiveInputArgs` (shared by the four read commands) and
  `ResetArgs` structs.
- `crates/tar-dedup/src/lib.rs`: `pub mod cmd;`.
- New `crates/tar-dedup/src/cmd.rs` + `cmd/{inspect,list,dump,query,reset}.rs`.
  `cmd.rs` owns the shared resolution helpers:
  - `resolve_catalog_source(&ArchiveInputArgs) -> Result<CatalogSource>` —
    archive-else-db, validates existence.
  - `resolve_reset_db(&ResetArgs) -> Result<PathBuf>` — `--db` xor `--work-dir`.
  - `not_implemented(name) -> Error` — the shared guard.
- `crates/tar-dedup-cli/src/main.rs`: thin match arms calling
  `tar_dedup::cmd::{inspect,list,dump,query,reset}::run(&args)`.

## Steps
1. cli.rs variants + arg structs.
2. cmd.rs helpers + the five command stubs.
3. lib.rs module decl; main.rs dispatch.
4. `plans/catalog-commands-scaffold.md` (this file).
5. Verify `cargo check -p tar-dedup`, `cargo check -p tar-dedup-cli`,
   `cargo check --features err-to-panic -p tar-dedup`.

## Concerns
- Read commands are read-only and take no fs4 lock; a mutating `reset` will need
  the work-dir `.lock` once implemented.
- The catalog readers will later open a raw read-only `rusqlite::Connection`
  against the footer-extracted DB (not the `Database` pipeline facade) — decided
  at implementation time.