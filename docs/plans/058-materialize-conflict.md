# Plan: out_tree placement collision detection (`MaterializeConflict`)

Status: agreed 2026-10-30. Small production gap found while scoping the
integration matrix: when `--transform` / `--strip-components` / relative mapping
collapse two **different** members onto the same `out_tree.abs_path`, the
`INSERT OR IGNORE` in `insert_out_tree_rows` silently drops the second member —
no row, no error, no record. `--no-strict-separation` same-file fanout
(same target, same `file_id`) is legitimate and must stay silent; only a
resident row with a *different* `file_id` is a true member collision.

## Change set

### 1. New error variant — `../../crates/tar-dedup/src/error.rs`
Add `FileStatError::MaterializeConflict { path: PathBuf, details: String }`:
- `#[error("materialize conflict at {path}: {details}")]`
- `kind()` → `"MaterializeConflict"` (this is the stored `errors.error_type`)
- `io_path()` → `Some(path)` (the collided target → `errors.abs_path`)
- `recreate()` falls through the existing `other` arm → `General` w/ same path+message
- add to `sample_error_variants` in the error.rs test module (+ kind assertion)

### 2. `errors.rs` — expose an inner (transaction-scoped) inserter
Refactor `insert_errors(conn: &mut Connection, …)` → thin tx wrapper over a new
`pub(crate) fn insert_errors_in(conn: &Connection, drafts) -> Result<u64>` with the
existing statement body. Lets `insert_out_tree_rows` record inside its own
transaction (nested tx not allowed in SQLite).

### 3. `db/place_prologue.rs` — `insert_out_tree_rows` rework
Signature: `insert_out_tree_rows(conn: &mut Connection, rows, enabled: bool) ->
Result<Vec<OutTreeId>>`.

- Drop `INSERT OR IGNORE` → plain `INSERT INTO out_tree (abs_path, file_id, flags)`.
- Run inside one transaction; prepare the out_tree insert + `errors` insert on the tx.
- Per row: on success → id; on `SQLITE_CONSTRAINT_UNIQUE` (`extended_code`), fetch the
  resident `(id, file_id)` for the abs_path and:
  - resident `file_id == row.file_id` **or** `row.file_id.is_none()` (scaffold dir /
    fanout) → benign: return resident id, no error;
  - otherwise → **collision**: if `enabled` (i.e. `!--no-errors`), insert a
    `RecordDraft { file_id: Some(row.file_id), out_tree_id: Some(resident),
    phase: ErrorPhase::Extract(PlacementPrologue), error:
    MaterializeConflict { path: target, details: "<src> conflicts with <resident>" },
    flags: default }` via `insert_errors_in`.
  - any other SQLite error propagates (FK/programming).
- Commit once. Return per-row resident id (unchanged contract).

### 4. Facade + call sites
`Database::insert_out_tree_rows(&self, rows, enabled)`. Callers:
- `place_prologue` populate dirs/rest (`populate_out_tree_abs` / `populate_out_tree_rel`):
  `enabled = !rt.config.process.no_errors`
- `ensure_parent` + test helpers (place.rs, permissions.rs, place_prologue.rs tests):
  `false`

### 5. Tests
- error.rs: `MaterializeConflict` in shape battery (`io_path` = target, kind).
- db/place_prologue tests: **parameterized over `enabled`** — two `NewOutTreeRow`s with the
  same `abs_path`, different `file_id`:
  - `enabled = true` → 1 `errors` row: `file_id` = displaced, `abs_path` = target,
    `error_type` = "MaterializeConflict", phase = `placement_prologue`; first wins in out_tree.
  - `enabled = false` → no `errors` rows; same out_tree outcome.
  - sanity: same `file_id` twice (fanout) → no errors even when enabled.
- Shell scenario `path_collision_transform` (later integration batch): transform maps two
  members to one target; assert first-wins content, second absent, and the run is
  identical under `--no-errors`.

## Files touched
`error.rs`, `db/errors.rs`, `db/place_prologue.rs`, `db.rs`,
(in tests) `db/place.rs`, `unarchive/place.rs`, `unarchive/permissions.rs`.

## Verify
`cargo test -p tar-dedup --lib` (incl. previously failing meta test) and
`--features test-all`.
## Implementation notes (2026-10-30) — DONE

- error.rs: `FileStatError::MaterializeConflict { path, details }` added (Display,
  `kind()` = "MaterializeConflict", `io_path()` = the collided target, `recreate()`
  degrades to `General` via the existing fallback arm); covered in the error.rs
  shape battery.
- errors.rs: split `insert_errors` into a thin transaction wrapper + new
  `pub(crate) insert_errors_in(&Connection, drafts)` so `insert_out_tree_rows` can
  record conflict drafts inside its **own** transaction (single commit — no drift
  possible between observing the UNIQUE violation and persisting the error).
- db/place_prologue.rs `insert_out_tree_rows(conn: &mut Connection, rows, enabled)`:
  `INSERT OR IGNORE` → plain `INSERT`; a `SQLITE_CONSTRAINT_UNIQUE` extended-code
  violation resolves to the resident row. Same `file_id` (fanout) / `file_id IS
  NULL` (scaffold dirs) → benign; different `file_id` + `enabled` → `MaterializeConflict`
  `RecordDraft` (file_id = displaced member, out_tree_id = resident, path = target)
  flushed via `insert_errors_in` in the same tx. Any other SQLite error propagates.
- `Database::insert_out_tree_rows(rows, enabled)`; populate call sites pass
  `!config.process.no_errors`; scaffold/test call sites pass `false`.
- Tests (parameterized): `materialize_conflict_recorded_and_gated_by_no_errors`
  (`enabled∈{true,false}` → 1 vs 0 `errors` rows; displaced file_id, target
  abs_path, kind "MaterializeConflict") and `same_file_fanout_stays_silent`.
- Verify: lib 689/0; `--features test-all` 714/0; release build green.
