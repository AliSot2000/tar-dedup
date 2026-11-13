# 091 — Cleanup DB timestamp + manifest-required bypass extract

Status: **implemented (2026-11-13).** `cargo test -p tar-dedup-lib --lib`: 773
passed. The full `test-all` run is deferred until the VM rebuild is online.

Two independent changes:

1. Give the retained cleanup DB a fixed, second-precision UTC timestamp.
2. Remove the legacy bypass upgrade (`tar-dedup.sqlite` → `manifest.sqlite`) and
   make `manifest.sqlite` mandatory for bypass extraction, with an explicit
   `--force` escape hatch for resuming from an existing work DB.

## 1. Cleanup DB timestamp

### Current behavior

`common/cleanup.rs::retained_db_path`:

```rust
let ts = Utc::now().to_rfc3339().replace(':', "-");
let name = format!("{ts}_{stem}.sqlite");
```

`to_rfc3339()` emits subseconds (`...17.123456789+00:00`) and then every `:` is
replaced. The timestamp is only a collision witness, not a semantic value, and
must not depend on logging/`--log-time`.

### New behavior

- Always UTC, whole seconds, `Z`:
  ```rust
  Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
  ```
  → `2026-11-13T14:20:17Z`.
- Name stays `{ts}_{stem}.sqlite`.
- Keep the colon form (`hh:mm:ss`) as the primary name.
- If the rename fails because the filesystem rejects the filename, retry with
  `:` substituted by `-` in the generated basename:
  ```text
  primary : 2026-11-13T14:20:17Z_<stem>.sqlite
  fallback: 2026-11-13T14-20-17Z_<stem>.sqlite
  ```
- Fallback trigger: `ErrorKind::InvalidInput` from `fs::rename` (EINVAL on
  Linux / invalid-filename on Windows). Do not probe the filesystem; attempt the
  rename and fall back only on that error. Other rename errors propagate.
- If the fallback also fails, propagate the fallback error.

### Code shape (`common/cleanup.rs`)

```rust
use std::io;                     // ErrorKind
use chrono::{DateTime, SecondsFormat, Utc};

fn retained_db_name(stem: &str, ts: DateTime<Utc>) -> String {
    format!("{}_{stem}.sqlite", ts.to_rfc3339_opts(SecondsFormat::Secs, true))
}

fn sanitized_db_name(name: &str) -> String {
    name.replace(':', "-")
}
```

In `cleanup_workdir`, compute primary + fallback and try the primary rename; on
`io::ErrorKind::InvalidInput` retry the fallback. Log the path actually used.

### Tests

- `retained_db_name` with a fixed `DateTime`: asserts seconds precision, no
  subseconds, `Z` suffix.
- `sanitized_db_name`: asserts only `:` is changed.
- Existing cleanup tests match `*_{stem}.sqlite`, so they keep working.
- Manual check on the NTFS volume (colon rejection → fallback) when convenient.

## 2. Bypass must have a manifest

### Current legacy path

`unarchive.rs::ensure_bypass_catalog` promotes a "pristine" `tar-dedup.sqlite`
to `manifest.sqlite` when the manifest is absent. That is the legacy behavior to
remove. A finished bypass product has `manifest.sqlite`; `tar-dedup.sqlite` is
the disposable extract work DB (a copy of the manifest) used to continue work.

### Decisions

- `manifest.sqlite` is required to start a bypass extract.
- `tar-dedup.sqlite` is never promoted to a manifest.
- Missing manifest:
  - **Resume** + `--force` + an `Incomplete` work DB → continue from the work DB.
  - Everything else → error.
  - `Fresh`/`Create` do **not** get the forced-resume exception.
- Config validation fails early unless `--force`; runtime enforces the stricter
  policy rule above.

### `config/paths.rs`

Add to `PathLayout` (import `SNAPSHOT_INIT_TAR_NAME`):

```rust
pub fn bypass_manifest_path(&self) -> PathBuf {
    self.work_dir.join(SNAPSHOT_INIT_TAR_NAME)
}
```

In bypass mode `work_dir` is the product dir, so this is
`<product>/manifest.sqlite`.

### `config/extract.rs`

At the **end** of `impl Validate for ExtractConfig::validate` (after the existing
`--no-rehash` bypass rule, so that test keeps its error):

```rust
if self.bypass_archive
    && !self.paths.bypass_manifest_path().is_file()
    && !self.process.force
{
    return Err(Error::Config(format!(
        "bypass product {} has no {}; re-run the archive, or pass --force to resume \
         from an existing {}",
        self.paths.archive_path.display(),
        SNAPSHOT_INIT_TAR_NAME,
        DB_NAME,
    )));
}
```

`--force` only passes this early check; runtime still applies the policy rule.

### `unarchive.rs` — rewrite `ensure_bypass_catalog`

Remove the `pristine` probe and the two `// TODO Remove with patch > v1.0.0`
comments around it. New logic:

```rust
let work_db = config.paths.db_path();
let manifest = config.paths.bypass_manifest_path();

if !manifest.is_file() {
    let force_resume = !reset_work_db
        && config.process.start_policy == StartPolicy::Resume
        && config.process.force
        && matches!(resolve_extract_work(&work_db)?.0, WorkPresence::Incomplete);
    if force_resume {
        tracing::warn!(
            "{} missing; resuming from {} (--force)",
            SNAPSHOT_INIT_TAR_NAME, work_db.display());
        return Ok(());
    }
    return Err(Error::Config(format!(
        "bypass product {} has no {}; re-run the archive{}",
        config.paths.archive_path.display(),
        SNAPSHOT_INIT_TAR_NAME,
        if config.process.start_policy == StartPolicy::Resume {
            " (or pass --force to resume from an existing work db)"
        } else { "" },
    )));
}

if reset_work_db || !work_db.is_file() {
    remove_work_db(&work_db)?;
    let tmp = work_db.with_file_name(format!("{DB_NAME}.tmp"));
    copy_file_reflink(&manifest, &tmp)?;
    std::fs::rename(&tmp, &work_db).map_err(|e| Error::io(&work_db, e))?;
}
Ok(())
```

`remove_work_db` stays (it is still needed for `--fresh` and materialization).

### `unarchive/scan.rs` — debug assert only

Do **not** repeat the manifest check at runtime. Add a precondition assert at the
top of `run_bypass` (add the `StartPolicy` import):

```rust
debug_assert!(
    rt.config.paths.bypass_manifest_path().is_file()
        || (rt.config.process.force
            && rt.config.process.start_policy == StartPolicy::Resume),
    "PRECONDITION: bypass manifest missing without a forced resume"
);
debug_assert!(
    rt.config.paths.db_path().is_file(),
    "PRECONDITION: ensure_bypass_catalog must materialize the work db"
);
```

Update the `run_bypass` doc comment: the manifest is mandatory except for the
explicit forced-resume case; the work DB is only a mutable copy. Keep skipping
`DB_NAME*` and `SNAPSHOT_INIT_TAR_NAME*` in the payload walk.

## Tests

- Delete `ensure_bypass_catalog_upgrades_pristine_legacy_db`.
- Rewrite `ensure_bypass_catalog_errors_without_usable_manifest`:
  - no manifest + no work DB → `has no manifest.sqlite`;
  - finished/dirty `tar-dedup.sqlite` alone → still `has no manifest.sqlite`;
    assert it is **not** renamed.
- Keep `ensure_bypass_catalog_fresh_rebuilds_work_db_from_manifest`,
  `bypass_archive_extract_round_trip`, and the Done-cleanup test.
- Add forced-resume cases:
  - `Resume` + `Incomplete` work DB + no manifest, no `--force` → error;
  - same + `--force` → Ok, work DB untouched;
  - `Fresh` + `--force` + no manifest → error;
  - `Create` + `--force` + no manifest → error.
- Config tests:
  - `bypass_archive_mode_resolves_work_dir_to_product_and_validates`: create a
    dummy `manifest.sqlite` in the product before `build`.
  - New: bypass + missing manifest → config error; with `--force` → builds.
  - `no_rehash_with_bypass_requires_force` is unaffected because the no-rehash
    rule runs before the manifest rule.

## Docs

- This file is the implementation plan.
- Add a one-line note to `docs/plans/088-bypass-product-manifest-db.md` that the
  pristine-DB upgrade is removed by 091.

## Verification

1. `cargo test -p tar-dedup-lib --lib` (target on tmpfs if the NTFS volume is
   still suspect).
2. Bypass round-trip, config, cleanup, and `ensure_bypass_catalog` subsets.
3. `cargo test --features test-all` on the VM once it is back online.
4. Manual NTFS check of the timestamp fallback (`--db-retention always` beside an
   NTFS archive).
