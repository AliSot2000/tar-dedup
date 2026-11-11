# 075 — File↔session ownership (kill the `archived`-regression)

Status: **agreed + implementing**. Scope settled:
- Full replace: purge `FileFlag::AppendedPath` (single source of truth).
- Merge `TarWriter::finalize_archive`/`finalize_tar_writer_session` into one `finalize(emit_tar_eof)`.
- Drop `promote_to_archived` in favor of the common `set_file_phase`.
- No migration (pre-1.0.0; `--fresh` wipes stale workdirs).

## Problem: promoted `archived` rows can regress via truncation

`end_session` (in `tar_builder.rs`) runs in this order:

```
promote_pending_archived()      // #1 — every AppendedPath row → 'archived'
append_snapshot(...)            // #2 — snapshot must show 'archived' (extract promotes on it)
writer.finalize_archive|session // #3 — io / FORCE interrupt can Err here
finalize_archive_session(id)    // #4 — marks the session finalized
```

Between #1 and #4 a **force interrupt during finalize**, an **io error**, or a
**process crash** leaves the affected rows `archived` + `AppendedPath` while the
session stays open. On the next startup `recover_incomplete_session` truncates
the archive to `open_session.archive_offset` and `clear_tar_writer_session_pending`
only resets **non-`archived`** rows — so the just-promoted rows keep `archived`/
`AppendedPath` but their payload bytes were truncated out. Extract then reports
them missing. We never want a once-`archived` file to lose its payload.

This is reachable via Ctrl-C×3 during finalize (the compress writer honors
`shutdown.check_in_flight`, force-only), not just by crash — so it is real, not
theoretical. Snapshot-before-finalize is required, so we can't avoid the promote
window by reordering; we must be able to **revert** the promoted rows when their
session is truncated.

### Clarification: the last in-archive snapshot is always "open"

`append_snapshot` does `checkpoint()` then copies the DB into the stream
**before** `finalize_tar_writer_session` runs. So every `snapshot.sqlite` — including
the final one — carries the current session as `finalized = OPEN (0)`. The
session is only marked finalized in the DB afterward, and **the footer is the only
place that persists the finalized/closed catalog** (at a seekable offset with the
integrity hashes). That's exactly why the footer exists. Consequence for this
design: `promote → archived` while the session is still `OPEN` is the *normal*
state at every final snapshot (required so extract can promote on it), so we do
NOT gate promote on "session finalized"; instead we make the revert session-scoped
and durable via `session_id`.

## Answer to "why two finalize fns"

`finalize_archive` (line 164) vs `finalize_tar_writer_session` (line 138) differ **only** in
`take_sink(emit_tar_eof)` — false for session, true for archive. Identical
otherwise. → **Merge** into a single `finalize(self, emit_tar_eof: bool)`.

## Why "don't honor shutdown in finalize" is insufficient

It closes the force-interrupt window but **not** io-error or crash — those still
leave the inconsistency, and the whole point of resumable cold-storage archiving
is crash-safety. So the durable fix is per-file session ownership.

## Design: `files.session_id`

Add `files.session_id INTEGER REFERENCES archive_sessions(id)` (nullable, plus an
index on it). Semantics mirror the old `AppendedPath` bitflag **plus** revert:

- Append success → `session_id = <current session>` (instead of setting the flag).
- Not yet appended / truncated-out → `session_id IS NULL`.
- `session_id IS NOT NULL` ⟺ old `AppendedPath` (payload is in a finalized,
  non-truncated session). So every `flags & AppendedPath` query maps to
  `session_id IS NOT NULL`.

**The fix** — `clear_archive_session_pending(truncated)` (called from recovery
after a truncate of open session `X`) becomes session-scoped and reverts
**archived** rows too:

```sql
UPDATE files SET
    session_id = NULL,
    phase     = CASE WHEN session_id = :truncated THEN 'staged' ELSE phase END
WHERE session_id = :truncated;
```

Rows promoted in the failed session are un-`archived` and re-appended; rows from
earlier finalized sessions (below the truncation offset) keep their payload.

### Clarification: `ErrorWhileArchive` is NOT reset, and is not selected

The revert matches only rows with `session_id = :truncated`. An
`ErrorWhileArchive` row was **never successfully appended**, so its `session_id`
is `NULL` — it is not in the truncated-session set and is left untouched (its
sticky failure persists, correctly). Consequently:

- `pull_pending_archive_rows` keeps its existing `ErrorWhileArchive` predicate
  (`:error_flag = 0`) unchanged; **only** the `AppendedPath` predicate becomes
  `session_id IS NULL`. They are independent filters.

## Replace `AppendedPath` (single source of truth)

Remove `FileFlag::AppendedPath` and migrate all consumers to `session_id`:

- `db/tar_writer.rs`: `mark_archive_session_pending(file_id, session_id)`;
  `promote_pending_to_archived` → `WHERE session_id IS NOT NULL`;
  `pull_pending_archive_rows` → `WHERE session_id IS NULL` (keep `:error_flag`);
  `clear_archive_session_pending(session)` (reverts archived rows of that session)
- `db.rs` facade signatures; `db/stats.rs` count
- `unarchive/scan.rs` + `db/scan.rs`: `count_missing_payloads`,
  `count_encoded_payloads`, `should_extract_canonical_id` → `session_id IS NOT NULL`
- `db/integrity.rs`: A7 / E1 / E2
- tests + `test_support.rs` that set/assert the flag

## Drop `promote_to_archived` (single-row) → `set_file_phase`

`promote_to_archived(conn, id)` is `UPDATE files SET phase='archived' WHERE id`
— identical to the common `set_file_phase(id, phase)`, just hardcoded. Drop it:
- `db/tar_writer.rs:189` + `db.rs:725` facade → removed.
- `archive/tar_builder.rs:141,171` → `rt.db.set_file_phase(record.id, FilePhase::Archived)?`.
  `set_file_phase` returns `()`; since it loses the `u64`/`bool` success signal at
  each new call site, annotate it `// TODO change return type` (do NOT do it now).

The **batch** `promote_pending_to_archived` stays (it's the mass end-of-session
promotion, a different function).

## Migration

`schema.rs` gains the column in `CREATE TABLE files` (+ index on `session_id`).
There is **no** `PRAGMA user_version` / migration infra in the repo; stale work
dirs are wiped with `--fresh` (the established stance — no production archives
pre-v1.0.0). No migration SQL needed.

## Files touched
`schema.rs`, `flags.rs`, `tar_writer.rs`, `db.rs`, `stats.rs`, `scan.rs`
(`db` + `unarchive`), `integrity.rs`, `archive/tar_builder.rs`, `tar_writer.rs`
(finalize merge), `test_support.rs` + the module tests that reference the flag.

## Tests
- **Regression**: simulate the failed-finalize window (rows promoted + session
  open) → run recovery → assert those rows revert to `staged`, `session_id NULL`,
  and prior finalized-session rows are untouched.
- Replace every flag-based unit assertion with the `session_id` form.
- `finalize` merge: archive emits EOF / session doesn't.
- Existing `footer_pending` / resume tests must still pass (unchanged).

## Verification
Local `cargo test -p tar-dedup-lib --lib`, then VM `--features test-all`.