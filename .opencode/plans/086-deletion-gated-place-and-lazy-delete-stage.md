# 086 — deletion-gated canonical place + `--lazy-delete-stage`

Status: **implemented.** `cargo check -p tar-dedup-lib` clean; `unarchive::place` 82/82 pass;
remaining lib failures are pre-existing (user's in-flight scan/stage/tar_builder rework +
id=0 dummy-row sentinel tests from the 085-era schema change; none touch this work).

## Motivation

Extract currently holds the whole stage dir (≥2× payload) until `cleanup_stage`
blind-deletes all canonical caches *after* the Files pass. We want to reclaim
space incrementally while copying, gated on per-canonical completion, with an
opt-out for slow/overloaded IO. Also fixes a resume bug: the pull predicates
only skipped `Placed`/`ErrorWhilePlace`, so a `Conflict`-skipped row was
re-pulled and reprocessed.

## 1. New flag + config

- `FileFlag::PayloadRemoved` (next free bit, 27) — canonical's cache payload
  deleted; idempotent/resume-safe.
- `PlacementOptions.lazy_delete_stage: bool`, CLI `--lazy-delete-stage`
  (default `false`). Threaded: `run` → `materialize_loop` (eager drain delete)
  and → `cleanup_stage` (final sweep), and into the delete helper.

## 2. Terminal-predicate fix (SQL)

A row is only **re-pulled** when done-state is absent:
`(Placed | ErrorWhilePlace | Conflict) == 0`. Apply to:
- `pull_materialize_queue` (Files)
- `list_out_tree_for_hardlinks` (Hardlinks)
- `list_out_tree_others` (Others)

Debug assert in `process_results`: every non-`Interrupted` outcome sets ≥1 of
`Placed ∨ ErrorWhilePlace ∨ Conflict`.

## 3. Canonical-resolution + delete helper (`db/place.rs`)

- Resolve `out_tree → file_id → files.canonical_id`.
- Gate: canonical's **own** out_tree row terminal (`Placed | ErrorWhilePlace |
  Conflict`). Sufficient: Hardlinks alias the placed output, Others read nothing.
- `delete_canonical_payload`: `remove_file(cache/content_id)` + set
  `PayloadRemoved`; idempotent.

## 4. Eager + final delete

- **Eager** (default): Files-pass drain, after `process_results`, delete the
  batch's terminal canonical caches (size-DESC order already from
  `materialize_queue`). Skipped when `lazy_delete_stage`.
- **Final**: `cleanup_stage` → gated sweep (PayloadRemoved-aware, same
  predicate, incremental id-walk). Skipped when `lazy_delete_stage`.

## 5. Lazy semantics

`--lazy-delete-stage` suppresses **both** eager and final delete within
`place::run`; end-of-run cleanup (`keep_stage`/workdir) is the only remover.

## 6. Hardlinks

Unchanged — link the placed canonical output, never the cache.

## Files

`db/flags.rs`, `db/place.rs`, `config/extract.rs`, `cli.rs`,
`unarchive/place.rs`, plus unit tests.

## Verification

`cargo test -p tar-dedup-lib --lib` (blocked on in-flight `place.rs`/`unarchive.rs`
rework — ask first), then VM `test-all`.