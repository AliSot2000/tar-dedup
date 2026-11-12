# 089 — Hash ingest performance

Status: **implemented (2026-11-12).**

## Symptom

Hash writes are noticeably slow. The workers stop being fed at regular
`drain_chunk` (4096) boundaries.

## Cause

`send_receive_loop` (`common.rs`) runs drain → tick → feed. The drain applies a
batch synchronously (`ingest_hash_outcome`), so feeding pauses for the duration
of one batch. Each hashed row runs (`db/hash.rs:229`):

```sql
UPDATE files SET sha1=…, sparse_count=…, phase='hashed'
WHERE (dev, inode) IN (SELECT dev, inode FROM files WHERE id = :id)
```

There is no index on `files(dev, inode)`, so the planner `SCAN files` for every
row: O(drain_chunk × n) per batch. (Confirmed with `EXPLAIN QUERY PLAN`: without
the index `SCAN files`; with `idx_files_dev_inode`,
`SEARCH files USING COVERING INDEX idx_files_dev_inode (dev=? AND inode=?)`.)

The transaction itself is already correct: `db/hash.rs:253` wraps each batch in
one transaction.

## Changes

1. **`db/schema.rs`** — add, beside the other `files` indexes:

   ```sql
   CREATE INDEX IF NOT EXISTS idx_files_dev_inode ON files(dev, inode);
   ```

   No migration and no `DB_VERSION` bump: `SCHEMA` is executed by
   `schema::initialize` on every open. Hash updates never change `dev`/`inode`,
   so the index adds no maintenance in the hot loop; it also accelerates
   `set_hardlink_canonicals` and the `dev/inode` integrity queries. It costs one
   extra insert per inventory row and slightly larger finalized DBs.

2. **Prepared statements** — stop re-parsing the per-row SQL 4096×:
   - `db/hash.rs::update_file_inspection_per_id`: `conn.execute` →
     `conn.prepare_cached(sql)?.execute(…)`.
   - `db/flags.rs::or_file_flags`: same (it already early-returns on empty
     flags, so the common hash path only pays the digest statement).

   `prepare_cached` keeps each SQL string in one place and reuses the compiled
   statement across the batch. Signatures are unchanged.

## Out of scope

- Decoupling the writer from the feeder (the synchronous apply is inherent to
  `send_receive_loop`, and the DB updates influence the drain).
- One `UPDATE … FROM` temp-table statement per batch.
- The phase-end `wal_checkpoint(TRUNCATE)` pause.

## Verification

Existing tests only; no new test for the index:

- `db::hash::tests::ingest_writes_digest_phase_sparse_count`
- `db::hash::tests::update_file_inspection_propagates_hardlink_group`
- `archive::hash::tests::modified_file_flagged`
- `cargo test -p tar-dedup-lib --lib`
- `scripts/vm-test.sh` (full `test-all`)
