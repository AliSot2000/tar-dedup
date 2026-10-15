# Plan: dedup-mode unit tests (Regular / Hash / None) + singleton fix

Status: **settled with the user (2026-09-26); implemented and green.** Add a
shared corpus test bed that exercises all three `--dedup-mode` values against
the exact same rows, plus one small production fix the tests require.

## Production fix (A)

`promote_singleton_filtered_to_deduped` (db/dedup.rs:597) currently sets
`phase = 'deduped'` only — a true singleton (unique `(sha1, size)`) would end the
phase without `canonical_id`, and `stage::promote_unstageable_files` /
`tar_writer::promote_ineligible_to_archived` would drop it. A unique-content file
IS its own canonical:

```sql
UPDATE files SET phase = 'deduped', canonical_id = id
WHERE phase = '<prev>' AND sha1 IS NOT NULL
  AND (sha1, size) IN (SELECT sha1, size FROM files
      WHERE sha1 IS NOT NULL GROUP BY sha1, size HAVING COUNT(*) = 1)
```

Plus a pinning db-level test (db/dedup.rs tests).

## Shared corpus (B)

`archive/dedup.rs` tests gain a `seed_mode_corpus(world) -> ModeCorpus` builder —
the **same rows** for every mode:

Ineligible (→ `deduped`, `canonical_id = NULL` in every mode):
- `non_file` — `ftype = 'dir'`
- `no_sha` — sha1 NULL
- `sha_err` — `ErrorWhileHash` flag set
- `filtered` — `include_reason_archive = 0` (after `apply_no_filter_archive`)

Eligible (distinct `(sha1, size)` per group so groups never merge):
- `singleton` — unique sha, 1 file
- `lone` — same sha, **binary different** content, ids L1<L2
- `pair` — same sha, binary equal, ids B1<B2
- `quad` — X,X,Y,Y (X≠Y), ids C1<C2<C3<C4
- `errgrp` — E1==E2 equal, **E3 chmod 000**, ids E1<E2<E3
- `hard` — same sha, real hard link, `dev=7 inode=99`, ids H1<H2

The builder sets `world.config.indexing.no_hardlink_detection = false` for
Regular / None-detect runs.

## Per-mode expectations (C)

**Regular** (`dedup_mode: Regular`):
- singleton → self · lone → both self canonical (lone-promotion) ·
  pair → B1 canon, B2→B1 · quad → C1 canon, C2→C1, C3 canon, C4→C3 (two
  canonicals) · errgrp → E1 self, E2→E1, E3 NULL + `ErrorWhileDedup` (root-skip)
  · hard → H1 self, H2→H1
- every row phase `deduped`; `count_check_with_canonical_completed()` == 0

**Hash** (`dedup_mode: Hash`) — one canonical per `(sha1, size)` = min(id):
- singleton → self; lone / pair / quad / errgrp / hard → min(id) canonical, all
  others point at it (errored file included; no error flag, pure SQL)

**None + detect on** (`None`, `no_hardlink_detection=false`) — only hard links
collapse: every eligible file self, except hard → H1 self, H2→H1

**None + no-detect** (`None`, `no_hardlink_detection=true`) — every eligible file
`canonical_id = id` (incl. both hard members); ineligible rows → NULL

## Files touched
1. `crates/tar-dedup/src/db/dedup.rs` — singleton fix + its db test.
2. `crates/tar-dedup/src/archive/dedup.rs` — `ModeCorpus`, `seed_mode_corpus`,
   4 mode tests.

## Verification
- `cargo build -p tar-dedup -p tar-dedup-cli`
- `cargo test -p tar-dedup --lib -- 'dedup'`
- full lib `cargo test -p tar-dedup --lib` — expect the 4 pre-existing failures
  + 1 ignored only
- no `--release`; no `cargo fmt`

## Notes / decision log
- `errgrp` needs chmod 000; root can still read it, so the Regular test early-
  returns under `geteuid().is_root()` (existing precedent:
  `run_fail_fast_on_errored_group`).
- The dedup test suites (archive + db) were *not* touched by the earlier
  dedup-mode work beyond the field renames; these tests are the first to cover
  `Hash`/`None` end-to-end.
- `promote_singleton_filtered_to_deduped` self-canonicalization is consistent
  with hash (MIN-over-one) and none (every-eligible-self) outcomes.