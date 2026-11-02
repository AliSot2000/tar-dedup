# 061 — Sidecar manifests + DB retention on error

Status: approved 2026-11-01 (design settled in conversation; implementation follows).

## Motivation

A filesystem may not reproduce the metadata carried in the archive (ACLs, SELinux,
ownership, xattrs, …). When permission application fails, the loss is only visible via
the persistent error log — which is useless in practice because the work DB is **deleted
by default** at cleanup (`cleanup_workdir`: non-`keep_stage` wipes work dir and DB;
`--keep-db` is the only escape hatch). This change adds:

1. Per-entry JSON **sidecar** manifests, written beside extracted entries when the
   filesystem could not carry some metadata (human-inspection convenience).
2. Tri-state **DB retention** keyed off recorded errors, so the machine-readable picture
   (the error log + records) survives a lossy restore.

Design note: on-error sidecars are a human-inspection convenience; the retained DB is the
source of truth. Once `inspect`-level DB inspection lands, sidecars become redundant.

## 1. `SidecarEntry` (`db/types.rs`)

Full down-conversion view near `FileRecord`/`StrippedRecord`, one pass from the record:

```rust
impl From<&FileRecord> for SidecarEntry { ... }
```

File side fields: `id, abs_path, ext, size, sha1 (hex), mtime/atime/ctime, uid, gid,
username, groupname, mode, ftype (as_str), device_id, inode, major, minor, xattrs,
posix_acl, selinux, win_perm, link_dst, new_name`.

- **No `canonical_id`** on the file side — it is internal dedup bookkeeping
  ("which payload row"), not a metadata fact.
- Out-tree embed, for reverse-mapping randomized filenames: **only `{ id, target_path }`**.
  `canonical_id` and `file_id` are dropped (the latter duplicates the top-level `id`).

## 2. Config surface

`cli.rs`, `config/process.rs`, `config/extract.rs`:

- `--sidecar {always|on-error|never}` — default **on-error**; `--no-sidecar` is an alias
  for `never`.
- `--hidden-sidecar` (bool) — toggles `{name}.json` ⇄ `.{name}.json`.
- `--db-retention {always|on-error|never}` — default **on-error**, shared archive + extract
  via `CleanupSettings`; **replaces `--keep-db`** (dropped, no alias). `--keep-stage`
  unchanged.
- `--no-errors` fold: `on-error` + `--no-errors` means nothing is recorded ⇒ counts are 0
  ⇒ effective `never`, with a **validation warning** (mirrors the existing
  `--no-overwrite-dir` / `--overwrite-all-dir` warn style in `config/extract.rs`).
- Known-issue policy (established): warning + restrictor wins. So link-tree + `on-error`
  ⇒ **warn + degrade to `never`**.

## 3. Naming / exhaustion

- Deterministic name `{name}.json` / `.{name}.json`. If occupied, append a random ASCII
  component: `{name}.{RAND}.json` / `.{name}.{RAND}.json`, up to
  `SIDECAR_MAX_NAME_ATTEMPTS` attempts. Never overwrite an existing file.
- Constant in `common/defaults.rs`: `pub const SIDECAR_MAX_NAME_ATTEMPTS: usize = 1_000_000;`
- Pure, seed-injected name generator (caller passes `u64`). Runtime seed =
  `SystemTime::now().as_nanos()` (same pattern as `has_valid_footer`); tests pass fixed
  seeds to drive the exhaustion path deterministically. No new dependencies, nothing
  cryptographic.
- Exhaustion ⇒ record `FileStatError::General("failed to find suitable sidecar name, no
  sidecar file written")` via the permissions recorder (which also feeds the on-error DB
  retention signal).

## 4. Permissions integration (`unarchive/permissions.rs`)

- Non-link mode: in `inner_apply_permissions`, write a sidecar per placed out_tree row
  (files, hardlinks, **directories**); `on-error` triggers on `!errors.is_empty()`.
- Link-tree mode: only `always|never` are accepted (no per-link permission errors exist;
  metadata is applied to the `.sources` canonicals). `always` walks
  `list_out_tree_for_linking` and writes a sidecar beside **each link** in the output tree,
  visible at the restore site (same treatment as non-link `always`).
- No auto-never code for "all metadata disabled": an untouched inode produces no errors,
  so `on-error` writes nothing naturally. Add a short intentionality comment; no config
  coupling.

## 5. DB retention plumbing

- `count_pipeline_errors(conn, CleanupMode)` in `db/errors.rs` — `CleanupMode { Archive,
  Extract }` is the preexisting Archive/Extract enum. Loop `PipelinePhase` for archive /
  `ExtractPipelinePhase` for extract and sum `count_errors(conn, Some(phase.as_str()))`.
  Exact-phase equality per row; a run's DB holds a single pipeline, so no cross-pipeline
  contamination.
- Before `cleanup::cleanup_workdir` (in `unarchive.rs` and the archive `run`), resolve the
  effective keep: `Always → keep`, `Never → drop`, `OnError → count_pipeline_errors > 0`.
  Kept DB continues to use the existing `kept_db_parent` copy mechanism.

## 6. JSON output

`serde_json::to_value(&entry)` then `serde_json::to_string_pretty(&value)`. serde_json's
default map is a `BTreeMap`, so keys serialize **alphabetically sorted for free**; pretty
output; no manual key ordering; no extra deps.

## 7. Tests

- Unit `db/types.rs`: `From<&FileRecord>` field mapping (sha1 hex, ext, ftype string),
  out-tree embed shape.
- Unit `permissions`: existence gate, hidden naming, `always`/`on-error`/`never` matrix,
  seeded-exhaustion ⇒ error recorded, `--no-errors` ⇒ `never` fold warning, link-tree
  `always` writes / `on-error` warns+degrades.
- VM integration: extract as non-root with `--same-owner` (deterministic EPERM) ⇒ sidecar
  appears **and** the DB survives (default `on-error`); paired scenario: zero errors ⇒ no
  sidecar, DB deleted; `--no-sidecar` ⇒ nothing written.
  `--no-errors` ⇒ effective `never`.

## 8. Files touched

`common/defaults.rs`, `db/types.rs`, `db/errors.rs`, `db.rs`, `cli.rs`,
`config/process.rs`, `config/extract.rs`, `common/cleanup.rs`, `unarchive/permissions.rs`,
`unarchive.rs`, `archive.rs`, `Readme.md`, `integration/scenarios/*`,
`docs/plans/061-sidecar-manifest.md`.

## Implementation order

1. Config: `SidecarMode`, `DbRetention` enums + flags; drop `--keep-db`; `--no-errors`
   fold + link-tree/`on-error` warnings in `config/extract.rs`.
2. `db/types.rs`: `SidecarEntry` + `From<&FileRecord>`.
3. `db/errors.rs`: `count_pipeline_errors(conn, CleanupMode)` (+ `db.rs` facade).
4. `unarchive/permissions.rs`: sidecar writer (seed, hidden, attempts, exhaustion error),
   non-link + link-tree integration.
5. Cleanup retention: resolve effective keep at run end (both pipelines).
6. Tests (unit + VM scenario) and `Readme.md` updates.
## Implementation notes (2026-11-01)

- `list_out_tree_for_linking` is **not** reused for link-tree sidecars: its
  progression is the place flags (`Placed` / `ErrorWhilePlace`), which the
  sidecar walk must not mutate and which would yield no meaningful set after the
  link tree is built. Instead `db/permissions.rs` gained
  `list_out_tree_links_for_sidecar(conn, last_id, batch_size)`: placed links
  (`o.flags & Placed`, paged by `o.id`) whose canonical `.sources` payload
  finished the permissions pass (`FileFlag::AppliedMetadata` /
  `ErrorWhileApplyingMetadata` on `can`) — the link-tree pendant of the non-dir
  walk, conditioned on `placed` rather than the `AtLinkSource` move marker.
- The sidecar walk pages manually (`last_id` cursor) because writing a sidecar
  does not mutate any flags; `batched_loop` would otherwise loop forever.
- CLI: `--sidecar {always|on-error|never}` (default on-error), `--no-sidecar`
  alias, `--hidden-sidecar`; `--db-retention {always|on-error|never}` replaces
  `--keep-db` on both archive and extract.
- Folds (warn-only, restrictor wins): `on-error` + `--no-errors`, and
  `on-error` + `--link-tree`, both degrade to `never`
  (`ExtractConfig::effective_sidecar_mode`).
- New unit tests: types view mapping + embed, claim deterministic/random/
  hidden/exhaustion, run-level always/on-error/never/fold/link-tree matrices,
  `count_pipeline_errors`, cleanup on-error retention. Lib suite 714/0.
