# Extract filtering — plan

Status: **proposal (schema + design locked, ready to implement)**.

## Goal

Add include/exclude filtering to the **extract** pipeline: select which members of an
archive get materialized, without re-archiving. Reuses the archive-side filter machinery.

## Design decisions (settled)

### Filter surface: `files.abs_path` (not `out_tree`)

- The filter matches the **original absolute member path** (`files.abs_path`), the same
  surface a future `tar-dedup list` command emits. list→filter round-trips exactly.
- out_tree `abs_path` is the *destination* (post `--strip-components`, `--transform`,
  rel/abs mode) — run-varying and not the stored member name. Filtering there would
  require "relative resolution in the head" and building rows before pruning them.
- The out_tree build query already gates on filter state (`db/place_prologue.rs:50`:
  `include_reason_archive < 0 AND exclude_reason_archive = 0`), so files-level filters
  prune before any out_tree/place work. Zero parallel machinery needed.
- Both surfaces are per-file-row; granularity is identical. out_tree "extra granularity"
  (select by destination) is a narrow compound case with `--strip-components`/`--transform`
  and can be a later secondary prune if ever needed.

### Schema: rename + add (no migration)

No schema migration is needed anywhere — **there is still no production archive**, so the
schema evolves in place and the next archive bakes the new shape into its manifest DB.

- **Rename** table `filter_reason` → **`filter_reason_archive`** (same DDL,
  `db/schema.rs:19-24`): `id` (`< 0` include, `> 0` exclude, `0` dummy), `source`, `line`,
  `expression`. Dummy row insert (`:137`) points at `filter_reason_archive`.
- **Rename** `files.include_reason` → **`include_reason_archive`** and
  `files.exclude_reason` → **`exclude_reason_archive`**
  (`db/schema.rs:69-70`, `REFERENCES filter_reason_archive(id) DEFAULT 0`).
- **Add** table `filter_reason_extract`, same shape + dummy id 0 row.
- **Add** `files.include_reason_extract`, `files.exclude_reason_extract`
  (`REFERENCES filter_reason_extract(id) DEFAULT 0`).

Rationale (unchanged): a file has exactly one include/exclude pointer; archive and extract
rules must coexist without overwriting each other, so extract state is its own table +
column pair. The extract DB is a copy of the manifest DB, so both rule sets live side by
side; a shared table would force a discriminator into every filter query.

### Composition semantics

Extract filters are **conjunctive on top of** the inherited archive gate. Effective
extractable = archive gate **AND** (no extract rules → allow all) **AND**
(`include_reason_extract < 0 AND exclude_reason_extract = 0`).

Extract filters can never re-include a file excluded at archive time — its content is not
in the archive.

### DB-less fallback (resolved)

If the scan found **no manifest DB** (truncated / non-conform archive), the extract filter
pass **does nothing**: no error, no constructed `files` rows. This is deliberate
compatibility for best-effort extraction of damaged archives, not a legacy path.

- Filters therefore live in `ExtractConfig` (from CLI args) and are ingested into
  `filter_reason_extract` **only once the scan has populated files**.
- If there is no DB content to filter, every tar member found is extracted as today.

### No `flags` column on `filter_reason_extract` (resolved)

Strictly minimal shape mirroring `filter_reason_archive`
(id/source/line/expression). No `flags`; a future `--why`/list-provenance column is
out of scope until it is actually needed.

### Shared filter SQL generator

New functions in `db/common.rs` (single source of truth for both pipelines):

- `generate_archive_filter(prefix: Option<&str>) -> String`
  → `{p}include_reason_archive < 0 AND {p}exclude_reason_archive = 0`
  (return is `String`, not `&str`, because the table-alias `prefix` is dynamic; the
  `prefix` mirrors the existing `SqlFileRow::sql_columns(prefix)` convention).
- `generate_archive_and_extract_filter(prefix: Option<&str>) -> String`
  → archive gate **AND** `{p}include_reason_extract < 0 AND {p}exclude_reason_extract = 0`.

Used at the *positive* gate sites; archive-side sites keep the archive-only generator,
extract-side sites (rehash, out_tree build/count) switch to the combined generator.

Note (pre-existing, out of scope): `db/filter.rs apply_no_filter` sets
`include_reason = 1`, which contradicts the `include_reason < 0` gates used by
hash/sparsify/stage/tar_writer/place. Flagged for a follow-up; this plan keeps whatever
semantics exist today and only generalizes the SQL fragments.

### Shared filter application helpers (`common/filter.rs`)

Move the pure filter mechanics currently living in `archive/filter.rs` into a new
`common/filter.rs` (declared in `src/common.rs`, NOT a `mod.rs`), parameterized so both
pipelines reuse them:

- `parse_filter`, `test_match`, `ParsedFilter`, `FilterResult`
- `ingest_filters` / `handle_filter` / `handle_query` — parameterized over which
  `add_include_pattern` / `add_exclude_pattern` closure and which `ErrorPhase` they target
  (archive `PipelinePhase::Filter` vs extract `ExtractPipelinePhase::Filter`).
- `fast_filter` batch loop — parameterized over `get_rows_to_filter` + `apply_filter_result`
  so extract can reuse it against the extract columns.

`archive/filter.rs` and the new `unarchive/filter.rs` become thin wrappers.

### Filter application point

New `ExtractPipelinePhase::Filter` variant, positioned between `ScanTar` and `Rehash`
(`config/phases.rs`, `unarchive.rs` phase loop). Extract already has every file
materialized in the DB, so filtering is a pure DB pass — no eager/lazy batching option.

Phase body:
1. No-op when the scan found no manifest (DB-less fallback above).
2. `clear_extract_filters` (delete non-dummy `filter_reason_extract` rows + reset the
   two extract columns) so the phase is idempotent on resume.
3. Ingest CLI/`ExtractConfig` patterns into `filter_reason_extract`
   (reuse `common::filter::ingest_filters`), applying `--anchored` / `--ignore-case`.
4. Batched regex match over `files.abs_path` writing `include_reason_extract` /
   `exclude_reason_extract` (reuse `fast_filter`).
5. Empty rule set → promote-all (analogous to `apply_no_filter`).

### Schema migration (none)

Extract columns/tables are added to `schema.rs` DDL directly. Every future archive's
manifest DB bakes them in; the extract pipeline copies the manifest as-is. **No**
`ensure_extract_filter_columns`, no `ALTER` in `normalize_installed_catalog`, no
work-DB-local migration.

## Implementation checklist

- [ ] `db/schema.rs`: rename `filter_reason` → `filter_reason_archive` (+ dummy insert);
      add `filter_reason_extract` (+ dummy insert); rename/add the four `files` columns.
- [ ] `db/common.rs`: rename columns in `FileRecord::sql_columns`/`from_row`; add
      `generate_archive_filter` + `generate_archive_and_extract_filter`.
- [ ] `db/filter.rs`: retarget all SQL from `filter_reason`/`include_reason`/
      `exclude_reason` to `*_archive`; add `add_include_pattern_extract`,
      `add_exclude_pattern_extract`, `count_filters_extract`, `get_filters_extract`,
      `apply_filter_result_extract` (targets extract columns), `apply_no_filter_extract`,
      `clear_extract_filters`.
- [ ] `db.rs`: facade methods for the extract filter set.
- [ ] `common/filter.rs` (new): move `parse_filter`/`test_match`/`ingest_filters`/
      `fast_filter` from `archive/filter.rs`, parameterized over target columns + phase.
- [ ] `archive/filter.rs`: shrink to a thin wrapper over `common::filter`.
- [ ] `config/phases.rs` + `unarchive.rs`: new `ExtractPipelinePhase::Filter` (`"filter"`)
      + phase-loop arm between ScanTar and Rehash.
- [ ] `unarchive/filter.rs` (new): phase run — DB-less no-op, clear, ingest, batched apply.
- [ ] `db/place.rs:146` + `db/place_prologue.rs:50,60`: switch extract-side positive gates
      to the combined `generate_archive_and_extract_filter` generator.
- [ ] `db/rehash.rs:22` + `db/scan.rs:182`: extract-side positive gates → combined generator.
- [ ] `cli.rs`: add `--include` / `--exclude` / `--include-from` / `--exclude-from`
      (`+ --anchored` / `--ignore-case`) to `ExtractArgs`; `ExtractConfig` carries them.
- [ ] List command (follow-up): emit `files.abs_path`; optional display-only relative flag.

## Open questions

None pending. (DB-less fallback behavior and the `flags` column were resolved.)