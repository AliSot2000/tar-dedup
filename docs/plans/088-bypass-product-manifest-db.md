# 088 — Bypass product: canonical `manifest.sqlite` + disposable extract work DB

Status: **implemented (2026-11-12).** The two open decisions were resolved as
recommended: bypass `--fresh` resets only the work DB (manifest/payloads kept),
and work-DB deletion at Done follows the existing `db_retention` default
(`on-error`).

Supersedes the product-DB naming in plan 082 (which says the bypass product dir
holds a finalized `tar-dedup.sqlite`).

## Motivation

In bypass mode the work dir *is* the product dir (`config/archive.rs:324`,
`config/extract.rs:420-434`). The archive bakes its work DB to
`<product>/tar-dedup.sqlite`, then extraction opens that same file and mutates it
heavily (scan flags, extract runtime state, `out_tree`, place/permission flags,
WAL checkpoints). The canonical archive catalog is therefore clobbered by any
extraction. We also want the work DB gone once extraction is done so a low-space
user does not pay for a second full catalog copy.

Split the two roles:

- **`manifest.sqlite`** (`SNAPSHOT_INIT_TAR_NAME`) — canonical, immutable archive
  catalog. Written once by archive; never opened for writing again.
- **`tar-dedup.sqlite`** (`DB_NAME`) — disposable extract work DB: a reflink/copy
  of `manifest.sqlite`, created at extract start, deleted when Done.

## Design

1. **Archive (bypass finalize).** The true last step (after the DB is closed and
   `cleanup_workdir` ran) renames `<product>/tar-dedup.sqlite` →
   `<product>/manifest.sqlite`. Rename is atomic; the connection close already
   checkpointed and removed `-wal`/`-shm`, so the file is self-contained.
2. **Archive rerun detection.** `tar-dedup.sqlite` no longer exists after a
   finished run, so presence must read `manifest.sqlite` and report
   `(WorkPresence::Done, ProductPresence::Finished)` for a valid bypass catalog
   in phase `Done`. Otherwise a rerun would treat the product as absent and
   re-archive from scratch.
3. **Extract start.** Ensure the canonical exists, then materialize the work copy:
   - if `manifest.sqlite` exists → use it;
   - else if `tar-dedup.sqlite` is a **pristine** bypass catalog (`get_bypass_archive()
     == Some(true)` and `load_extract_runtime_state()` is `None`) → rename it to
     `manifest.sqlite` (upgrades legacy products and closes a crash window between
     archive-Done and archive-rename);
   - else → hard error: bypass product has no `manifest.sqlite`; re-run archive.
   Then, only if `tar-dedup.sqlite` is absent, copy `manifest.sqlite` →
   `tar-dedup.sqlite.tmp`, then atomic `rename` → `tar-dedup.sqlite`.
4. **Extract payload walk.** `scan::run_bypass` must skip both `DB_NAME*` and
   `SNAPSHOT_INIT_TAR_NAME*`; otherwise `manifest.sqlite` trips the unknown-file
   stray gate (warn / `--fail-fast` / keep-stage escalation).
5. **Done cleanup.** `cleanup_workdir` deletes `tar-dedup.sqlite` per
   `db_retention`; it must never touch `manifest.sqlite`. With the default
   `on-error`, a clean run deletes the work DB and the product is left pristine
   (payloads + `manifest.sqlite`); an errored run keeps the work DB for debugging.
6. **Freeing the source.** Deleting the product payloads is the existing
   `StageKeep` path: bypass extract already permits `--no-keep-stage`
   (`config/extract.rs:514-529`, warns "deleting the stage results in the source
   being deleted"), and `place::cleanup_stage` (`place.rs:1268`) deletes each
   consumed payload after place; the end cleanup `remove_dir_all`s the product.
   `--lazy-delete-stage` defers the payload sweep to that end cleanup. No new
   mechanism.
7. **Copy helper.** Reuse the existing reflink→copy idea. `copy_single_file`
   (`place.rs:945`) is coupled to `FileId`/`Shutdown`/progress, so add a tiny
   `common/files.rs::copy_file_reflink(src, dst)` = `reflink::reflink(..)` else
   `fs::copy`, and use it for manifest → tmp (then rename). Leave
   `copy_single_file` untouched.

## Change map

| File | Change |
|---|---|
| `archive.rs` | `finalize_bypass_product()`: if bypass and `db_path` is a file and `manifest` is absent → `fs::rename` to `SNAPSHOT_INIT_TAR_NAME`; call after `drop(db)` + `cleanup_workdir` (`:192/:207`). Presence probe (`:44-72`): bypass + absent `db_path` → validate `manifest.sqlite` (`Done`) as Finished. |
| `config/archive.rs` | No change (bypass already forces `keep_stage`/`db_retention = Always`, `:420-449`). |
| `unarchive.rs` | Add `ensure_bypass_catalog()` before `resolve_extract_work` (`:70`): upgrade pristine legacy DB, then copy manifest → work DB if absent. Fresh handling per §7. |
| `unarchive/scan.rs` | `run_bypass` copy is centralized in `unarchive.rs`; update the payload-walk skip (`:281`) to `DB_NAME` + `SNAPSHOT_INIT_TAR_NAME` prefixes. |
| `common/files.rs` | New `copy_file_reflink(src, dst) -> Result<()>` (reflink, fallback `fs::copy`). |
| `common/cleanup.rs` | No behavior change required; optionally make the bypass "kept database" log manifest-aware. |
| `common/defaults.rs` | Fix `DB_NAME` doc comment: the baked catalog is now `manifest.sqlite`. |
| `cmd.rs` | `--work-dir` read commands (`:43-52`): prefer `tar-dedup.sqlite`, fall back to `manifest.sqlite` (after Done the work DB is gone). |
| `docs/plans/082-…` | Leave historical; this plan supersedes the product-DB naming. |

## Ordering guarantees (the failure modes)

| Concern | Guarantee |
|---|---|
| Rename while DB open / un-checkpointed WAL | After `drop(db)` (archive `:192`); `run_bypass_archive` checkpointed at `tar_builder.rs:369`; close removes `-wal`/`-shm` |
| Rerun after finalize | Presence reads `manifest.sqlite` (§Design 2) |
| Crash between Done and rename | Extract `ensure_bypass_catalog` upgrades a pristine `tar-dedup.sqlite` |
| Crash mid-copy | Write `tar-dedup.sqlite.tmp` then atomic rename; walk skips the tmp via the `DB_NAME` prefix; a leftover tmp is overwritten next run |
| `manifest.sqlite` seen as stray | Walk skips `SNAPSHOT_INIT_TAR_NAME*` |
| Cleanup race with rename | Rename is the last step, after `cleanup_workdir`; cleanup only touches `db_path` + temp files |
| `--fresh` deletes the product | Open decision §7.1 |

## Open decisions (resolved)

1. **`--fresh` on bypass extract.** Resolved (a): bypass `--fresh` deletes only
   `tar-dedup.sqlite`(+`-wal`/`-shm`/`.tmp`) and re-copies from
   `manifest.sqlite`; payloads and the manifest are never wiped.
2. **Work-DB deletion at Done.** Resolved: rely on the existing `db_retention`
   default (`on-error`: clean run deletes, error run keeps). Legacy handling is
   "upgrade pristine `tar-dedup.sqlite` → `manifest.sqlite`, else error".

> Superseded by `091-bypass-manifest-required-and-cleanup-timestamp.md`: the
> legacy pristine-DB upgrade is removed; `manifest.sqlite` is mandatory, with an
> explicit `--force` resume from an incomplete work DB as the only exception.

## Tests / verification

- `archive.rs::bypass_archive_writes_dir_product_and_sets_meta` (`:404-436`):
  assert `product/manifest.sqlite` (not `tar-dedup.sqlite`); open it; payload walk
  skips `manifest.sqlite`.
- `unarchive.rs::bypass_archive_extract_round_trip` (`:375-443`): after archive
  assert `manifest.sqlite`; after extract assert `manifest.sqlite` still present
  and (retention permitting) the work DB removed.
- `integration/scenarios/bypass_archive_roundtrip.sh`: assert `manifest.sqlite`
  after archive and that the product survives (default) / is consumed with
  `--no-keep-stage`.
- New unit tests: rerun-after-finished rejection; work-DB deleted at Done;
  legacy pristine DB upgraded; `--fresh` keeps payloads+manifest (per §7.1);
  `--no-keep-stage` removes product payloads; copy preserves `manifest.sqlite`.
- Full suites: default `--lib`, `--features test-all` on the VM via
  `scripts/vm-test.sh`.
