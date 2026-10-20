# Plan: scan phase ends with every row `unarchived`

Status: **agreed (2026-10-06).** Follow-up to `sanity-checks.md`: the scan phase must
end, like every other phase, with the whole `files` table in its phase (`unarchived`).
Failures are carried by the flags (`FileExtracted` / `ErrorWhileExtracting`), never by
sticking a row in an earlier phase.

## Goal

After a **successful** scan-loop exit:
`count_files_in_phase(Unarchived) == count_entries()`.

Remainders are partitioned into precise, actionable buckets and either triage-promoted
(force) or turned into an explicit panic (no force).

## Mechanics

1. **Normalize at install (`db/scan.rs::normalize_installed_catalog`):**
   - Keep clearing `FileExtracted` (fresh catalog).
   - **Stop collapsing `staged` → `archived`.**
   - Blanket-promote every **non-participant** → `unarchived` immediately:
     rows in `inventoried|hashed|filtered|deduped|sparsified|archived` that are
     dup (`canonical_id IS NULL OR != id`), non-file, sha-less, or archive-filter-failed.
   - **Participants** (self-canonical, `ftype='file'`, `sha1 NOT NULL`, archive filter
     passed) stay in their install phase: `staged` (leading-manifest provenance) or
     `archived` (footer provenance). This keeps the corrupted-payload case visible in
     `archived` at end-of-scan.

2. **Payload extracted** → `FileExtracted` (unchanged). Failed cache copy under `--force`
   → `ErrorWhileExtracting` (B2, already in place).

3. **Snapshot confirms the catalog:** `apply_snapshot_promote_unarchived` first promotes
   `staged` → `archived` for rows listed as archived in the snapshot, then runs the
   existing `archived` → `unarchived` (FileExtracted / canonical fan-out) promotions.
   End-of-scan `promote_extracted_to_unarchived` stays as the footer-provenance catch-all
   (`archived ∧ FileExtracted → unarchived`).

4. **Tar member names are always UTF-8:** remove the `Ok(None)` skip branch in
   `entry_name`; convert non-UTF8 names lossily, so they reach the `(other, _)` arm and
   panic with `Error::Config("tar member … neither a catalog nor a content id …")`.
   `--relax-utf8` has no effect on extract members anymore (stays for archive-side walks).

5. **`last_member_was_snapshot`** in `ScanState` (persisted meta key): `true` iff
   the most recently processed member was `snapshot.sqlite`. Message enrichment only —
   **not** a correctness gate (multi-session counter-example: a mid-archive session
   snapshot can be the last member while later-session candidates stay `staged`).

## End-of-scan gate (`unarchive/scan.rs::run_inner`)

After `check_cached_payloads`, `report_scan_completeness`, and the normal
`promote_extracted_to_unarchived`:

1. **D — excluded by extract filter** (eager mode only): `archived` ∧
   `NOT(generate_archive_and_extract_filter)` → promote to `unarchived`, emit an
   informational line, **not** a violation.
2. Tally the violations of the remaining non-`unarchived` rows:
   - **A — tail truncated / unconfirmed:** `staged` (payload may already be extracted).
   - **B — extraction failure:** `archived` ∧ `ErrorWhileExtracting`.
   - **C — payload not found in the archive stream:** `archived` ∧ no
     `ErrorWhileExtracting` ∧ no `FileExtracted` ∧ not filter-excluded (head/middle
     truncated or otherwise fabricated archive).
3. If A+B+C > 0:
   - `--force` → `warn` with the full breakdown, record a session error, blanket-promote
     the remainders → `unarchived`.
   - no `--force` → `panic` with the same breakdown.
4. `debug_assert!(count_files_in_phase(Unarchived) == count_entries())`.

Message (both the warn and the panic use identical text):

```
INVARIANT VIOLATION after scan: {n} row(s) not in 'unarchived':
  - {a} file(s) never confirmed by a final snapshot/footer (still 'staged';
    last tar member was {snapshot|other}, from_footer={bool})
  - {b} file(s) failed to extract their payload (ErrorWhileExtracting)
  - {c} file(s) were marked as archived/added but their payload was not found
    in the archive stream
Run with --force to proceed (affected files will be skipped).
```

(D is reported separately before the violation check:
`{d} file(s) excluded by the extract filter (not extracted by design)`.)

## Files

- `db/scan.rs` — normalize (keep `staged`, non-participant blanket);
  `apply_snapshot_promote_unarchived` (`staged → archived` step);
  add `promote_scan_remainders` (`staged/archived → unarchived`);
  add `tally_scan_remainders(conn, eager)` → `(staged, extracting_err, missing_member, filter_excluded)`;
  `promote_extracted_to_unarchived` unchanged; persist/load `last_member_was_snapshot`.
- `db/extract.rs` — `ExtractScanState.last_member_was_snapshot: bool`.
- `db/meta.rs` — `scan_tar_last_member_was_snapshot` key + getter/setter.
- `db.rs` — facade entries for the new db/scan.rs helpers.
- `unarchive/scan.rs` — `entry_name` lossy (no skip); set `last_member_was_snapshot` per
  member; end-of-scan gate replacing the B1 block; `store_progress_in_db` persistence.
- `unarchive/scan.rs` tests + `db/scan.rs` tests (see below).

## Tests

- normalize: keeps participant `staged`/`archived`; promotes dup/non-file/sha-less/
  filter-failed → `unarchived`; clears `FileExtracted`.
- snapshot confirm: `staged → archived` then `archived+FileExtracted → unarchived`.
- end-gate bucket A (force `warn`+sweep vs no-force `panic`); bucket B; bucket C
  ("marked archived/added but payload not found"); bucket D excluded+promoted (not a
  violation); multi-session truncation counter-example (snapshot last, later-session
  candidates `staged`).
- `entry_name`: lossy name fails `parse_content_id` → `(other, _)` → `Error::Config`.
- existing scan tests updated to the new normalize/gate semantics.

## Build / verify

`cargo build -p tar-dedup` then `cargo test -p tar-dedup --lib` (dev profile only), then a
CLI archive→extract smoke run.