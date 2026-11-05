# 068 — Premature promotions: empty files (dedup) + hash-error phase advance

Status: **agreed (2026-11-05), implementation round pending.** Two fixes in the
archive pipeline, both "premature promotion passes" for entry classes that a
phase counts but otherwise cannot process.

## Context

Found while archiving `/mnt/4tb/Scratch Tar-Dedup/Polybox` (16k files, 2,875
dirs, 54 zero-byte files) with `--fresh --xz-extreme`:

```
thread 'main' panicked at crates/tar-dedup-lib/src/archive/dedup.rs:341:9:
dedup finished with 54 file(s) still in hashed (expected 0 after skips + rounds)
```

Root cause: the 54 stragglers were all **0-byte files** (size = 0, sha1 =
`da39a3ee…`, the empty-string digest). They form one duplicate `(sha1, size)`
group, but the group-state table `dedup_progress` declares
`size INTEGER NOT NULL CHECK(size > 0)` (`db/dedup.rs:97`) and
`populate_temp_table` inserts with `INSERT OR IGNORE` (`db/dedup.rs:129`), so
the `size = 0` group is **silently dropped** — the Regular FSM never sees it,
the files stay `hashed`, and `sanity_check_flags` panics.

Audit of the rest of the pipeline found no sibling defects in sparsify/stage/tar
or extract scan/rehash (they all use full-negation promotes or explicit salvage
gates, with "covers every or-arm" tests). One **second** defect in the same
class surfaced: unreadable files are silently omitted in eager-filter mode.

## Pass 1 — Dedup: promote empty files as one canonical group

New `db/dedup.rs::promote_empty_files_to_dedup(eager_filter)`:

```sql
UPDATE files SET phase = 'deduped', canonical_id = (
    SELECT MIN(id) FROM files AS g
    WHERE g.ftype='file' AND g.sha1 IS NOT NULL AND g.size = 0
      AND g.phase = '<prev>'          -- prev_phase(eager_filter)
)
WHERE phase = '<prev>' AND ftype='file' AND sha1 IS NOT NULL AND size = 0
```

- One canonical per empty-content group (`MIN(id)`); a lone empty file
  collapses to itself (`MIN` over one row).
- Called in `archive/dedup.rs::run` **immediately after**
  `promote_ineligible_entries_to_dedup` (line 33), before mode dispatch, so
  Regular/Hash/None behave identically.
- Effect: empty files leave `hashed` before the FSM; the `size = 0` group no
  longer needs a `dedup_progress` row; `count_pending_dedup_groups()==0` no
  longer fires the premature `sanity_check_flags` panic; phase totals
  (`count_dedup_phase_total`) naturally exclude the promoted rows.

Rationale (vs. alternatives): relaxing the `CHECK(size > 0)` would drag 54
empty files through trivial worker comparisons for no benefit; the
ineligible-promote idea (`OR size = 0`) would set `phase` *without* a
`canonical_id`, routing normal files down the no-payload/error archive path.

## Pass 2 — Hash: `sha_error` is an outcome, so advance the phase

Problem (found in same audit): `ingest_hash_outcome` (`db/hash.rs:247`) sets
`ErrorWhileHash` on failure (`:268`) but does **not** advance `phase` — only the
`Ok` arm writes `phase='hashed'`. With the default `eager_filter = true`
(`config/filter.rs:56`), dedup's prev-phase is `'hashed'` while the errored row
is parked in `'filtered'` (hash's own prev-phase). `promote_unhasheable_files`
has no `sha_error` arm, `count_pending_hashable_files` excludes such rows
(`db/hash.rs:197`), and dedup's `promote_ineligible_entries_to_dedup` only
sweeps `hashed` — so an unreadable/deleted-at-hash file is **never archived
(not even metadata-only)**, silently missing except for an `errors` row. The
existing "tolerates an unreadable file" test (`archive/hash.rs:402`) never
asserts the file's phase, hence the blind spot. The *non-eager* dedup test
`promote_non_ineligible_and_singleton` (`db/dedup.rs:1374`) proves the intended
end-state — `sha_error` → `deduped` → archived metadata-only — by seeding the
row in dedup's prev-phase, which the eager pipeline never reaches.

Fix:

1. **`db/common.rs`**: rename `mark_phase` → `set_file_phase` (same body,
   `UPDATE files SET phase = :phase WHERE id = :id`; mirrors `set_file_flag`
   naming). Update callers:
   - `db/sparsify.rs:208,215`
   - `db/rehash.rs:201,205,209` (+ unit-test refs `:308,:362,:419`)
   - `global_mark_phase` stays (distinct batch helper).
2. **`db/hash.rs::ingest_hash_outcome`**: in the `Err(he) =>` →
   `Error::FileStat(_)` arm, alongside `set_file_flag(ErrorWhileHash, true)`,
   add `set_file_phase(…, FilePhase::Hashed)`. `Interrupted` arm unchanged
   (in-flight fragment stays pending); `Ok` arm unchanged. Both mutations sit
   in the existing transaction, so the phase+flag change is atomic.

Result: errored rows reach dedup's prev-phase (`hashed`) → the existing
`sha_error` arm of `promote_ineligible_entries_to_dedup` (`db/dedup.rs:590`)
sweeps them → `deduped` (canonical NULL) → sparsify/stage/tar non-canonical
promotes → **archived metadata-only** + recorded error. No re-queue risk
(`pull_pending_hash_rows` already filters `sha_error`, `db/hash.rs:113`);
integrity check A1 (`ErrorWhileHash ⟺ sha1 IS NULL`,
`db/integrity.rs:139`) remains satisfied.

## Tests

- `db/dedup.rs`: empty duplicate group + empty singleton → prev-phase drains to
  0, `MIN(id)` canonical elected, `run()` ordering honored.
- `db/hash.rs` ingest: a `FileStatError` outcome now **also asserts
  `phase='hashed'`**; `Interrupted` leaves phase untouched.
- `archive/hash.rs` "tolerates an unreadable file": add the phase assertion for
  the bad file (closes the blind spot).
- Roundtrip (`test-all`): tree with ≥2 zero-byte files + one `chmod 000` file
  → run completes without panic; errored file archived metadata-only and
  present in `errors`; extract restores the empty files.

## Out of scope

- Extract-side handling of metadata-only (unreadable-at-archive) files —
  pre-existing semantics.
- ACL/SELinux `ENOTSUP` (os error 95) error flood observed on the failing
  filesystem — 38,950 recorded `acl`/`selinux` errors per run; candidate small
  fix: treat ENOTSUP as "no ACL/SELinux, skip silently". Separate item.

## Files touched

- `crates/tar-dedup-lib/src/db/dedup.rs` (new promote + tests)
- `crates/tar-dedup-lib/src/archive/dedup.rs` (call the promote)
- `crates/tar-dedup-lib/src/db/common.rs` (rename `mark_phase` → `set_file_phase`)
- `crates/tar-dedup-lib/src/db/sparsify.rs`, `db/rehash.rs` (callers of the rename)
- `crates/tar-dedup-lib/src/db/hash.rs` (ingest phase-advance + tests)
- `crates/tar-dedup-lib/src/archive/hash.rs` (phase assertion in the unreadable-file test)