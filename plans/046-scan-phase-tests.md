# Plan: scan-phase unit-test battery

Status: **agreed (2026-10-20).** `unarchive/scan.rs` — `run_inner`, `process_entry`,
`extract_canonical_member`, `validate_result`, `captured_extract_database`,
`install_database`, `entry_name`. Test the provenance/resume/footer/integrity/abort
matrix plus the error arms, folded in with the existing in-module tests.

## Implementation notes (2026-10-20)

Built; `unarchive/scan.rs` at 93.3% region / 93.4% line (from ~77.8%), 36 tests in the
module, full lib suite 438 passed / 1 failed (the pre-existing
`clear_archive_meta_drops_only_byte_counters` blocker). `archive_footer.rs` gained its
first test module (6 tests) covering `verify_archive_integrity` + `read_footer` directly.
Corrections to earlier notes (re-verified against the code):

- **`entry_name` debug_assert IS reachable**: `Path::file_name()` returns `None` for a
  trailing `..`/`.` (so those arms return `Err`), but on Unix `\` is a literal filename
  char, so `Path::new("a\\b")` survives `clean()`/`file_name()` intact and trips the
  `contains('\\')` debug_assert. Added `entry_name_debug_assert_on_backslash_filename`.
- **`verify_archive_integrity`'s `Err` arm is directly testable** (no mocking): corrupt
  the leading MAGIC at `offset` → `locate_footer` returns `Err(Config)`. It is only
  *unreachable through `run_inner`*, where `read_footer` runs first and aborts on the
  same corruption. Added the direct tests to `archive_footer.rs`.

Deviations from the plan that stand:

1. **`from_footer` on a footer-installed DB**: the `Some(edb)` branch must still learn
   it came from a footer this pass, or the in-stream manifest member re-triggers
   `install_database` on an already-drained filter buffer (panic). Applied as a single
   `scan.from_footer |= footer_this_pass;` after the scan-state init (persisted for
   resume by the end-of-scan save). The footer path had never been exercised before.
2. **Payload-only archive without `--force` is rejected at the first-member gate**
   (`Err(Config)`), not via the `rt.db.is_none()` panic — that panic and the
   `process_entry (false, true)` panic are unreachable (no-force forces a leading
   manifest, which installs a DB). See the explanation below.
3. **Graceful does not interrupt a copy**: `check_in_flight` only raises on FORCE
   (graceful stops between members). The mid-copy test uses `request_force`.
4. **Progress-seed assertion dropped**: `ProgressBarSet` exposes no position getter, so
   the resume `>10` test asserts end-state (`count_extracted_canonical` +
   `last_member_index`) instead.
5. **Sparse caveat**: the comparison asserts byte-identical content and
   `sparse_blocks <= regular_blocks`, requiring a strict reduction only when the
   regular copy stays fully allocated. On this machine the fs does NOT auto-sparsify
   plain zero writes (8 MiB → 16384 blocks), so the strict branch fires and sparse-cp's
   hole-punching is observable.

## Why the `rt.db.is_none()` no-force panic is unreachable

The only ways `rt.db` becomes `Some` are `open_resume_database`, footer install, or
`install_database` (manifest / force-snapshot). `process_entry`'s **first-member gate**
forces every other first member to abort when `!force`:

- `(SNAPSHOT_INIT, false)` → `install_db` → DB installed (Some).
- `(SNAPSHOT, false)` → `!force` → `Err`; `force` → installs DB.
- `(content_id, false)` → `!force && !saw_first` → `Err` (first-member gate, line ~554).

So with `!force`, the first member either installs a DB or the scan aborts; a state of
`saw_any_members == true && db.is_none() && !force` cannot occur, which is exactly what
the `panic!` guard asserts. With `--force` it IS reachable (payload-only stream) and
returns `Err("Archive did not contain database")` (tested).

## Why a copy is only interruptible by FORCE

`extract_canonical_member`'s `on_bytes` closure calls `shutdown.check_in_flight()`,
which returns `Err(Interrupted)` **only when the mode is FORCE**; a graceful request
returns `Ok` (it is a between-units stop, not an in-flight abort). `copy_reader_batched`
and `sparse_copy_reader` both propagate that error (the former removes the partial
destination). So mid-copy aborts require `request_force` — which is what
`scan_force_interrupt_mid_copy_returns_interrupted` uses.

## Deferred: function mocking (mockall + `#[double]`)

Explicitly **not** introduced in this plan. The only scan arm that stays uncovered is
`verify_archive_integrity` returning `Err` *through `run_inner`* (scan.rs:220-230):
unreachable with real files because the same corruption makes `read_footer` fail first,
and the real-world trigger is the archive being mutated between the two reads (a race).
Its behavior is behaviorally identical to the tested `Ok(false)` arms (record session
error; `force → warn+continue` / `no-force → return Err`), so end-to-end coverage of the
decision tree already exists; the delta is only which `Error` variant propagates.

Deferred follow-up if we ever want that arm (or general race injection): add
`mockall` + `mockall_double` as a dev-dependency and route the two footer fns through a
narrow seam so `run_inner` keeps calling a stable path. Notes from research:

- `#[double] use crate::archive_footer::integrity;` swaps the **module name binding** at
  compile time (`cfg(test)` only) — calls must be written `integrity::verify(...)` in the
  module holding the doubled `use`; bare `verify(...)` imports or absolute paths
  (`crate::archive_footer::...::verify(...)`) always hit the real fn.
- Module/static fns are **global expectations** (a scoped `Context`); with no
  expectation set they panic — they do **not** fall through to the real impl. So module-
  mocking toggles between mock behaviors, never mock↔real.
- For per-test mock↔real toggling on the same path, prefer a **trait/generic injection**
  (e.g. `trait Integrity { fn verify(&self,..); fn read_footer(&self,..); }` with
  `RealIntegrity` + `MockIntegrity`), which makes the toggle a value instead of a
  global. `faux` is a poor fit here: it only mocks public methods on structs (no free
  functions), and intercepts at the instance level only.

Also reachable *without* mocking if ever wanted: `read_footer`'s `Err(FileStat)`
downgrade arm (scan.rs:193-199) via a bogus/nonexistent archive path (falls back
footer-less, then `open_tar_archive` errors).

## Goal

Systematically cover the scan phase entry points and error arms. Where a case is already
tested, extend rather than duplicate; where the code structure makes an arm unreachable,
note it and skip (do not fake it).

## Existing coverage (keep)

- `scan_caches_payloads_and_promotes_on_snapshot` — manifest→payload→snapshot happy path.
- `scan_no_db_success/failure_buffered_then_flagged_on_snapshot` — force no-DB window,
  snapshot installs DB + drains `force_buffer`.
- `scan_force_payload_failure_marks_error_flag_and_promotes` — bucket B.
- `scan_missing_member_triaged_under_force` — bucket C.
- `scan_truncated_tail_staged_triaged/panics_without_force` — bucket A warn vs panic.
- `scan_partial_snapshot_staged_panics_without_force` — mid-archive checkpoint.
- `scan_interrupt_persists_state_and_resume_skips_processed_members` — interrupt/resume.
- `entry_name_lossy_converts_invalid_utf8`.

## New test-support helpers (`test_support.rs`)

1. `build_multi_catalog(dir, n)` — `n` self-canonical archived files (+1 duplicate) with a
   matching payload tar built as `manifest + n×payload + snapshot`. Enables resume with
   `>10` extracted and filter/eager tests.
2. `build_footer_archive(dir)` — `build_archive` + `archive_footer::write_footer`.
3. `content_id_name(fid, sha, size)` — mint a `{hash_b64}.{fsize_b64}.{fid_b64}.ext`
   member name for a *stray* cache-dir file (not in any catalog).
4. `corrupt_byte(path, offset)` — flip a byte for integrity tests.

## Test cases

### run_inner — resume (extend existing)
- Resume DB with **0** extracted: run once, then roll back DB to
  `last_member_index=0`, `FileExtracted` cleared, phases reset; re-run → re-extract all.
- Resume DB with **1** extracted (roll back to leave exactly one `FileExtracted`).
- Resume DB with **>10** extracted (`build_multi_catalog(dir, 12)`), assert progress seed
  and that already-extracted rows are not re-copied (phase position / flag check).
- Merge with the existing interrupt/resume test into one parametrized sibling set sharing a
  "roll DB back to N extracted" helper.

### run_inner — footer (new)
- Fresh footer start: `from_footer=true`, `saw_manifest_db=false`, manifest member not
  installed, rows end `unarchived`.
- Integrity `Ok(true)` → success.
- Integrity `Ok(false)` (corrupt 1 byte before footer): `--force` → warn+continue,
  no-force → `Err(Config)`, session error recorded.
- Integrity `Err` (corrupt footer magic/offset → `locate_footer` errors): force → warn,
  no-force → `Err`.
- `read_footer` `Err(FileStat)` branch (scan.rs:193): needs a `FileStat`-typed footer
  failure; all realistic footer failures are `Config` → **skip** (note in code).

### run_inner — abort / edge paths
- Empty archive (line 336): zero-member tar → `Err("Archive is Empty")`.
- `rt.db.is_none()` after members (lines 344-364): tar with only a canonical payload (no
  manifest/snapshot/footer). force=true → `Err("Archive did not contain database")` and
  cache dir removed (unless `keep_stage`); force=false → `catch_unwind` panic.
- Stray file in `extract_cache_dir` (`check_cached_payloads`, line 371): pre-create a file
  whose name is a valid content-id not in the catalog → `new>0` → no-force panic,
  force warn.
- Filters × eager: `build_multi_catalog(dir, 2)` + `config.filter.exclude/include_patterns`
  with `eager_filter` on/off; assert which payloads got `FileExtracted` and bucket D
  promotion.

### validate_result (direct unit tests — pure fn)
- `(false,false,false,false)` → `catch_unwind` panic.
- Provenance violations `(false,false,false,true)`, `(false,false,true,_)`,
  `(true,false,true,_)`: force=false → `Err(Config)`; force=true → `Ok`.

### process_entry (prefer `run()` with crafted archives)
- manifest not first (`SNAPSHOT_INIT`, `saw=true`): `[payload(force), manifest]` → Err.
- snapshot first, no force → Err; snapshot first, force → DB installed (body = catalog).
- valid content-id first, no force → Err; force → buffered (already covered).

### extract_canonical_member
- Eager filter gate — via the filter test above.
- Sparse vs regular copy: member with long zero runs; `config.scan.sparsify` on/off;
  compare extracted file's sparse/allocated size.
  **Caveat:** the filesystem/kernel may sparsify zeros transparently, making sparse-cp and
  regular copy indistinguishable on the final artifact. If the hole count / allocated size
  does not differ, fall back to asserting both copies produced a byte-identical cache file
  and note the sparsify-vs-regular distinction as not externally observable in this
  environment (keep the `sparse-cp` math tests as the real proof).
- Mid-copy interrupt: large payload + thread calling `shutdown.request_graceful()` during
  the copy → `Err(Interrupted)`.

### captured_extract_database (direct unit tests — pure fn)
- Ok; io::copy Err (custom `Read` failing after N bytes → error recorded); File::create Err
  (dst = existing directory path → error recorded).

### entry_name (direct)
- Ok (normal name); Err (`Path::new("/")` → no file_name); debug_assert (`Path::new("..")`
  → `catch_unwind` panic in debug).

## Skipped / unreachable (do not test)

- `archive.entries()` map_err (line 265), `wrapped_entry` map_err (lines 280/286): need a
  tar stream that yields a per-entry error; not reachable with well-formed fixtures.
- `entry.path()` → `continue` (line 292): only errors on invalid UTF-8 on Windows;
  unreachable on Linux.
- generic `Err(e)` in `extract_canonical_member` (line 652): only `Interrupted` /
  `FileStat::Io` are realistic, both covered elsewhere.
- process_entry `(false, true)` panic (line 524): unreachable — no-force + no-DB forces
  the first member to be a manifest.
- `install_database` "three branches": the error arm needs an invalid-sqlite temp DB,
  which the pipeline can't produce from a valid tar; only the Ok path is exercised via
  the existing manifest-install tests.
- `read_footer` `Err(FileStat)` (line 193): see above.

## Implementation order

1. `test_support.rs` helpers (`build_multi_catalog`, `build_footer_archive`,
   `content_id_name`, `corrupt_byte`).
2. Pure-fn tests: `validate_result`, `entry_name`, `captured_extract_database`.
3. `run_inner`: resume (3 states), footer + integrity (4 cases), empty, no-DB abort,
   stray-file, filters × eager.
4. `process_entry` leftovers (manifest-not-first, snapshot-first ± force).
5. `extract_canonical_member`: sparse-vs-regular (with the fallback caveat), mid-copy
   interrupt.
6. `cargo test -p tar-dedup --lib` (dev profile; never `--release`).
