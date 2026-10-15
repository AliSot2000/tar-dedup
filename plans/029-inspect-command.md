# Inspect command — "show the archive the way it is"

`inspect` never claims to give a perfect representation: it reads what is
available and reports the rest as missing. Something there → shown; something
missing → empty line (text) / `null` (JSON). No normalization or promotion is
ever applied to an opened DB — it is treated **as is**.

## Catalog sourcing

Order of precedence:

1. `--db` / `--work-dir` → open that work DB as-is (`catalog_source = "work-db"`).
2. `-f ARCHIVE` → `archive_footer::read_footer(archive, temp)`; valid footer →
   `catalog_source = "footer"`.
3. No valid footer → **peek only the first tar member** (`open_tar_archive`
   with suffix-inferred compression):
   - first member is `manifest.sqlite` → open it as-is
     (`catalog_source = "leading-manifest"`);
   - otherwise → `catalog_source = "none"`, `head_truncated = true`, and the
     report still carries `first_member`, `archive_size`, and a `note`
     (corrupt-footer message when the magic/sha was off, else the first-member
     evidence), plus `member_counts = null`.
4. `--force` is **accepted but unimplemented** (documented): later it will scan
   the **entire** archive (not just the first member) and prefer the **last**
   `snapshot.sqlite` as the catalog. Never hard-error: a degraded report is
   always emitted.

## CLI (`ArchiveInputArgs` — shared by inspect/list/dump/query)

- `--force` — stub (catalog-level, future "prefer last snapshot").
- `--json` — single machine-readable JSON document (pretty by default).
- `--no-pretty` — compact JSON (no wrapping/indent; with `--json`).
- `--collapse` — text only; join records within a section with `; ` on one line.
- `--only <SUBSTR>` — case-insensitive substring match on the section header;
  emit only matching sections (drives the copy-paste map workflow).

## Report structure

Text: `--- {Header} ---`, then `key: value` records (db-dump style), blank
line after. JSON: field names are the lowercased headers; missing → `null`.

| Header / JSON key | Source |
|---|---|
| Tool Version / `tool_version` | `meta.tar_dedup_version` |
| OS / `os` | `meta.tar_dedup_os` |
| Archive Started At / `archive_started_at` | `MIN(started_at)` over **finalized** `archive_sessions` (RFC3339 UTC) |
| Archive Completed At / `archive_completed_at` | `MAX(finished_at)` over finalized sessions |
| Snapshot Taken At / `snapshot_taken_at` | `meta.archive_snapshot_taken_at` |
| Phase / `phase` + Session Count / `session_count` | `archive_phase` or `extract_phase` |
| Tar Writer Bytes In / Out | `meta.tar_writer_bytes_in/out` |
| Owner / Group | identity overrides |
| Owner Map / Group Map | `IdentityMap` → `from to` lines (map-file grammar; `+id` for id keys, `#` header comment) — copy-paste round-trip |
| Mode Changes / Transform | `meta.archive_mode_changes` / `archive_transform` |
| Include Filters / Exclude Filters | `filter_reason_archive` patterns, one per line, internal catch-all skipped |
| Archive Arguments / `archive_arguments` | stored `ArchiveConfig` (JSON) |
| Extract Arguments / `extract_arguments` | stored `ExtractConfig` (JSON) |
| Archive Size / `archive_size` | `stat` of the target archive (`-f` path, else stored config path) |
| Catalog / `catalog` | nested `{ source, head_truncated, first_member, member_counts, note }` |

## Stats (`db/stats.rs`, new; facade wrappers in `db.rs`)

| Header / Key | Query |
|---|---|
| Counts by File Type / `counts_by_ftype` | `GROUP BY ftype` |
| Included / `included` | `include_reason_archive < 0` |
| Excluded by Include / `excluded_by_include` | `include_reason_archive = 0` |
| Excluded by Exclude / `excluded_by_exclude` | `exclude_reason_archive > 0 AND include_reason_archive < 0` |
| Unfiltered / `unfiltered` | `include_reason_archive IS NULL` |
| Hashed / `hashed` | `sha1 IS NOT NULL` |
| Hash Errors / `hash_errors` | flag `ErrorWhileHash` (sanity-equivalent proxy) |
| Canonical Singletons / `canonical_singletons` | self-canonical rows whose `(sha1,size)` group has exactly one member |
| Canonical Files / Errors | `canonical_id = id` / flag `ErrorWhileDedup` |
| Sparsified Files / Errors | `HasSparse` / `ErrorWhileSparsify` |
| Bytes (all / canonical / archived) | `SUM(size)` / `canonical_id = id` / flag `AppendedPath` |

## Implementation

- `cli.rs` — new `ArchiveInputArgs` fields.
- `db/stats.rs` — raw SQL + typed results; `db.rs` — `mod stats;` + facade
  methods + meta getter facades (`get_archive_phase`, `get_extract_phase`,
  `get_archive_snapshot_taken_at`, `get_tar_writer_bytes_in/out`).
- `cmd/inspect.rs` — `InspectReport` (`#[derive(Serialize)]`), catalog
  resolution (footer / first-member peek / work-db / degraded), stats gather,
  temp file handling via `tempfile`, and the two renderers.
- No tests were added (per standing instruction); verification = existing suite
  + build + manual runs of `inspect` (footer, manifest, head-truncated, work-db,
  `--json`, `--json --no-pretty`, `--collapse`, `--only owner map`).

## Notes
- Timestamps: stored started/finished are RFC3339 **UTC**
  (`Utc::now().to_rfc3339()`); reported verbatim; interrupted/unfinalized
  sessions excluded.
- The full-archive scan (`--force`) is intentionally a later incremental:
  plumbing is shared with `unarchive/scan.rs` but the snapshot-promotion
  semantics conflict with "treat as is", so it stays stubbed.