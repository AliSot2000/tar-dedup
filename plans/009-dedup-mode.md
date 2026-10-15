# Plan: `--dedup-mode {regular|hash|none}` (replaces dead `no_dedup: bool`)

Status: **decisions settled with the user (2026-09-25).** Replace the inert
`no_dedup: bool` (CLI + `ArchivePipelineOptions`) — it is **dead code** today
(no reader in `archive/dedup.rs::run`, so `--no-dedup` does nothing) — with a
functional three-value `DedupMode`:

- `regular` — the current byte-compare FSM (unchanged default).
- `hash` — trust the digest: group by `(sha1, size)`; `min(id)` of each group
  becomes self-canonical, every other member points at it. No byte compare,
  no workers.
- `none` — no content dedup:
  - hardlink detection **on** (`--no-hardlink-detection` absent): group by
    `(sha1, size, dev, ino)`; `min(id)` self-canonical, members point at it
    (only genuine hard links collapse).
  - hardlink detection **off** (`--no-hardlink-detection` set): every
    hash-eligible file is self-canonical (`canonical_id = id`).

User decisions:
- CLI = **only** `--dedup-mode` (drop `--no-dedup`).
- `none` + no-hardlink-detection sets `canonical_id = id` for the whole eligible
  corpus (sparsify/stage/tar key on `canonical_id = id`, and the ineligible
  promote already ran — correct).
- Resume/serde compat = **ignore**: assume **no production archives exist**, so
  breaking an older work DB's `archive_config` JSON read is fine (`--fresh` is
  the documented recovery).
- **Do not write tests in this round** — production change only, import/test
  reviews come later.

## Current state (verified)

- `ArchiveArgs.no_dedup: bool` + `--no-dedup` flag — cli.rs:346-348.
- `ArchivePipelineOptions.no_dedup: bool` — config/archive.rs:74; set in
  `try_from` (:314) and `DEFAULT_CONFIG` (:450); merged via `merge_pick_clone`
  (:361).
- `ArchiveConfig` is persisted as `archive_config` meta (serde_json) and loaded
  on resume (main.rs:29, db/meta.rs). Field rename breaks old work DBs — accepted.
- `dedup::run` (archive/dedup.rs) does: promote-ineligible → promote-singletons →
  `create_temp_dedup_table` → FSM loop (workers + `dedup_progress`/`dedup_inflight`)
  → sanity + `drop_temp_dedup_table`. Only `promote_non_ineligible_entries_to_dedup`
  + the FSM election write `deduped`/`canonical_id`.
- Downstream: sparsify candidate predicate and stage `list_files_to_stage` /
  `promote_unstageable_files` key on `canonical_id = id`; tar writes
  self-canonical rows as payloads, dups (`canonical_id != id`) reference them.
- cli enum pattern to copy: `ConflictPolicy`/`HardLinkGrouping` live in cli.rs with
  `ValueEnum, Serialize, Deserialize, Default` and `#[value(rename_all = "kebab-case")]`.
- `prev_phase(eager_filter)` (db/dedup.rs:63), `FileFlag::ErrorWhileHash`,
  `generate_archive_filter(None)` are the building blocks for the new SQL.

## Design

### 1. `DedupMode` enum (cli.rs, mirrors `ConflictPolicy`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum DedupMode {
    /// Byte-compare duplicates within (sha1, size) groups (current behavior).
    #[default]
    Regular,
    /// Trust the digest: min(id) per (sha1, size) group is the canonical;
    /// no byte verification.
    Hash,
    /// No content dedup. With hardlink detection, only (sha1, size, dev, ino)
    /// hard-link groups collapse; without it, every eligible file is self-canonical.
    None,
}
```

### 2. CLI arg — replace the bool

`ArchiveArgs.no_dedup: bool` → `pub dedup_mode: DedupMode`:

```rust
/// Deduplication strategy.
#[arg(
    long = "dedup-mode",
    value_enum,
    default_value_t = DedupMode::Regular,
    help_heading = "Process Options"
)]
pub dedup_mode: DedupMode,
```

Drop the `--no-dedup` flag block.

### 3. Config wiring

- `ArchivePipelineOptions { dedup_mode: DedupMode }` (replaces `no_dedup`).
- `try_from`: `dedup_mode: args.dedup_mode`.
- `DEFAULT_CONFIG`: `dedup_mode: DedupMode::Regular`.
- `merge_over` unchanged (enum merges via `Default`/`PartialEq`).

### 4. `db/dedup.rs` — bulk canonical assignment (new)

Shared eligibility fragment (mirrors the FSM's candidate set):

```sql
phase = {prev_phase(eager)} AND ftype = 'file' AND sha1 IS NOT NULL
AND (flags & :error_hash) = 0 AND {generate_archive_filter(None)}
```

New functions + `Database` facades, all idempotent (guarded on `phase = <prev>`):

- `promote_hash_mode_to_dedup(conn, eager_filter) -> Result<u64>` (1 tx):
  1. elect: `UPDATE files SET canonical_id = id, phase='deduped'
     WHERE <eligible> AND id IN (SELECT MIN(id) FROM files WHERE <eligible>
     GROUP BY sha1, size)`;
  2. link: `UPDATE files SET canonical_id = (SELECT MIN(id) FROM files g2
     WHERE g2.sha1 = files.sha1 AND g2.size = files.size AND <g2 eligible>)
     WHERE phase = <prev> AND <eligible> AND canonical_id IS NULL`.
  (Row 2's subquery includes the elected row, so `MIN` is stable; the
  `canonical_id IS NULL` guard keeps newcomers from re-pointing elected rows.)
- `promote_none_mode_to_dedup(conn, eager_filter, detect_hardlinks) -> Result<u64>`:
  - `detect_hardlinks == false`: `UPDATE files SET canonical_id = id,
    phase='deduped' WHERE <eligible>`.
  - `detect_hardlinks == true`: two steps like hash mode, but `GROUP BY
    sha1, size, dev, ino` **with `dev IS NOT NULL AND inode IS NOT NULL`** on
    both elect and link (NULL dev/ino must not collapse unrelated files); then a
    fallback `UPDATE files SET canonical_id = id, phase='deduped'
    WHERE <eligible> AND (dev IS NULL OR inode IS NULL)`.

Counts: generalize `count_dedup_phase_total`/`count_dedup_phase_position` to the
eligible corpus, or add `count_dedup_eligible_total`/`position` (position =
already-`deduped` among `<eligible>`).

### 5. `archive/dedup.rs::run` — dispatch on mode

Keep the `promote_non_ineligible_entries_to_dedup(eager)` preamble + global
credit for **all** modes, then branch:

```rust
match rt.config.pipeline.dedup_mode {
    DedupMode::Regular => { /* existing FSM body unchanged */ }
    DedupMode::Hash | DedupMode::None => run_simple(rt, mode),
}
```

`run_simple(rt, mode)` (worker-less):
- `detect_hardlinks = !rt.config.indexing.no_hardlink_detection;`
- compute eligible total/position; `set_phase_total` / `set_phase_position`.
- dispatch: `Hash` → `promote_hash_mode_to_dedup(eager)`; `None` →
  `promote_none_mode_to_dedup(eager, detect_hardlinks)` — `inc_both(n)`.
- leftover-`prev_phase` assert (same message as today's tail), tracing info,
  `Ok(())`.
- No temp tables, no workers/channels, no `CheckWithCanonicalCompleted` sanity.
- Break out of the pre-dispatch FSM setup: hash/none modes skip
  `promote_singleton_filtered_to_deduped` (the elect covers singletons),
  `create_temp_dedup_table`, counting helpers that touch `dedup_progress`.

## Files touched

1. `crates/tar-dedup/src/cli.rs` — `DedupMode` enum; replace `no_dedup` arg.
2. `crates/tar-dedup/src/config/archive.rs` — field rename, `try_from`,
   `DEFAULT_CONFIG`; `use crate::cli::DedupMode`.
3. `crates/tar-dedup/src/db/dedup.rs` — the 2 (+1 fallback) SQL fns.
4. `crates/tar-dedup/src/db.rs` — `Database` facades for the new fns.
5. `crates/tar-dedup/src/archive/dedup.rs` — mode dispatch in `run`.
6. `crates/tar-dedup/src/archive/hash.rs`, `archive/sparsify.rs`,
   `archive/dedup.rs` — test-world `ArchivePipelineOptions { no_dedup: … }`
   literals → `dedup_mode: DedupMode::Regular` (keeps the files compiling).
7. `Readme.md` — replace the `--no-dedup` row with `--dedup-mode regular|hash|none`.

## Verification (this round — no new tests)

- `cargo build -p tar-dedup -p tar-dedup-cli` (prod green; only pre-existing warnings).
- `cargo test -p tar-dedup --lib -- 'dedup'` — existing `Regular` suite must stay
  green (mode default = Regular).
- Full lib `cargo test -p tar-dedup --lib` — expect only the 4 pre-existing
  failures + 1 ignored.
- Manual smoke (optional): seed two identical files, `--dedup-mode hash` → one
  payload; `--dedup-mode none` → both self-canonical.
- Never `--release`; no `cargo fmt`.

## Notes / decision log

- `--no-dedup` removed (no prod archives; breaking change accepted). README's old
  `--no-dedup` semantics = new `hash` mode.
- `promote_singleton_filtered_to_deduped` sets phase only; the new elect SQL
  covers singleton self-canonicalization, so hash/none modes don't need it.
- Pre-existing gap to verify (not this round): a *true* singleton in `regular`
  mode may end `deduped` with `canonical_id = NULL` (the dedup run test seams
  u1/u2 with the same sha byte, so they're a compare group, not singletons). The
  new modes do not inherit it (they self-canonicalize every eligible row). Follow
  up later.
- Field rename breaks resume of older work DBs (`archive_config` JSON). Accepted:
  no production archives exist; `--fresh` is the documented recovery.
- Tests deliberately deferred to a later round per user instruction.