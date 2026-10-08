# CLI completion for final-remainder TODOs 40–48

Agreed design for the last big CLI gap before the first release. Adds the
missing archive/extract inputs + validation, threads the tunable constants, and
adds a log-level / progress-mode knob. No new tests (verification = existing
suite + builds).

## Block association

| # | CLI flag | Block (subcommand) | Config field |
|---|----------|--------------------|--------------|
| 40 | `--use-shards`, `--max-shard-size` | Process Options (beside `--dedup-mode`) | `ArchivePipelineOptions.use_shards`, `.max_shard_size` — scaffold: runtime `Error::Config("shard-based dedup not implemented yet")` on `--use-shards` (mirrors `--exclude-vcs`) |
| 41+42 | `--ordering {size, name, id*}` (*gated `#[cfg(feature = "debug-ordering")]`) | Process Options | `ArchivePipelineOptions.order: ArchiveOrdering` |
| 43 | `--auto-flush-limit`, `--batch-size`, `--work-capacity`, `--out-capacity`, `--feed-chunk`, `--drain-chunk` (beside `--jobs`/`--io-jobs`) | Process Options | `ProcessOptions.{auto_flush_limit, batch_size, work_capacity, out_capacity, feed_chunk, drain_chunk}` |
| 44 | `--log-level {off,error,warn,info,debug,trace}` | Global (`global = true` on `Cli`) | → `init_tracing(level)` |
| 45 | `--progress-mode {none,global,per-thread}` (default `per-thread`) | Process Options | `ProcessOptions.progress_mode: ProgressMode` |
| 46 | `--force` | Process Options | `ProcessOptions.force` |
| 47 | `--relax-utf8` | Process Options | `ProcessOptions.relax_utf8` |
| 48 | `--create-other-entries` (**off by default**; mknod privilege note) | Placement (new) / extract | `PlacementOptions.recreate_none_file_entries = flag` (default flips true→false) |
| 48 | `--clean-target` (opt-in, default false) | Overwrite Control / extract | `PlacementOptions.clean_target` |
| 48 | `--link-source` | Link Tree / extract | `PlacementOptions.link_source` |
| 48 | `--no-reflink` | Placement (new) / extract | `PlacementOptions.no_reflink` |
| 48 | `--hard-link-grouping` (existing, **add default** `Global`) | Overwrite Control / extract | `PlacementOptions.hard_link_grouping` (wire) |
| 48 | `--absolute-links` (existing) | Link Tree / extract | wired; validation only |

## ProgressMode semantics (default `per-thread`)
- `per-thread` — current: global + phase + `push_sub_bar`s + per-thread worker bars.
- `global` — global + phase + `push_sub_bar`s; `create_thread_bars` is a no-op
  (workers report to the phase bar).
- `none` — every bar hidden (`ProgressBar::hidden`), clean stdout for piping.

## Validation (config `try_from` + pair validators)
- Archive + extract: `--relax-utf8` ⊥ `--force` and ⊥ `--fail-fast`.
- Tunables: each must be `> 0`.
- Archive: `--max-shard-size > 0` iff `--use-shards`.
- Extract:
  - `--absolute-links` requires `--link-tree`; ⊥ `--use-hard-links`.
  - `--link-source` requires `--link-tree`.
  - `--clean-target` ⊥ `--no-create-dirs` (replaces the `place.rs` assert) and
    warns when `--one-top-level` is absent.
  - `--hard-link-grouping` becomes optional, default `Global`.

## Implementation steps
1. `cli.rs`: new args/headings; `default_value_t = Global` on
   `hard_link_grouping`; global `--log-level` on `Cli`.
2. `config/*`: `ArchiveOrdering` (+ cfg'd `Id`), `ProgressMode`, new
   `ArchivePipelineOptions`/`ProcessOptions`/`PlacementOptions` fields; update
   `DEFAULT_CONFIG` consts, `merge_over`, both `try_from`, and the test
   constructors.
3. Thread tunables: replace const reads in `hash.rs`, `dedup.rs`, `sparsify.rs`,
   `rehash.rs`, `place.rs`, `place_prologue.rs`, `permissions.rs`, `filter.rs`
   (both), `tar_builder.rs` with `config.process.*`; set
   `Recorder::set_flush_limit`.
4. Ordering: `populate_archive_queue` takes the enum; `tar_builder.rs` reads
   config; `Id` variant → `ORDER BY id`.
5. Progress: `ProgressBarSet` gains `ProgressMode`; gate hidden bars and
   `create_thread_bars`.
6. `Cargo.toml`: add `debug-ordering` feature.
7. `main.rs`: `Cli::parse()` before `init_tracing(level)`.
8. Verify: `cargo test -p tar-dedup --lib`, `cargo build -p tar-dedup-cli`.

## Notes / behavior changes
- `recreate_none_file_entries` default flips to `false` (was hardcoded `true`);
  special entries (symlink/fifo/dev/socket) are only materialized when
  `--create-other-entries` is given. Document in Readme/TODO.
- Tunables and progress/force/relax-utf8 persist in the stored work-DB config;
  `resume` override surface unchanged (jobs/io-jobs/exit-after-stage).
- Shards are schema-only scaffolding; `--use-shards` errors out until implemented.