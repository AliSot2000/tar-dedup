# Plan: unified new_name mapping + out_tree re-cleaning

Status: **implemented (2026-10-20).** `place_prologue.rs`. Unify the three placement modes
(abs / rel / new_name) so name mangling operates on the **same relative target path**
the regular abs/rel mapping produces, and re-run the full cleaning program (clean +
strip_leading_up) on `new_name` values before they land in the `out_tree`.

## Mode model

| Mode | Target derivation | Placement |
|------|-------------------|-----------|
| **Abs** | `abs_path` drop leading `/` | `root.join(...)` |
| **Rel** | `abs_path` strip source prefix → add clamped rel prefix → clean → leading-up removed | `root.join(...)` |
| **NewName** | same rel path as abs/rel (pre-mangling), then transform + strip | `root.join(clean+strip_leading_up(new_name))` |

The pre-mangling input to transform/strip is **identical** to what abs/rel produce for
the same entry, so a transform matches the same way with or without transform mode.

The out_tree builder re-applies `clean` + `strip_leading_up` to every `new_name` before
writing, so even a transform that injects `../` cannot escape the extraction root
(resolves the residual `..` edge).

## Changes (`place_prologue.rs`)

### Shared helpers (single source of truth)
- `abs_rel_path(abs: &Path) -> PathBuf` — `abs.strip_prefix("/")`.
- `clamped_rel_prefix(original_path: &Path) -> PathBuf` — the clamped, relative prefix:
  absolute → strip `/`; `/` → empty; relative → `strip_leading_up(...).0`.
- `rel_rel_path(source_abs, rel_prefix, abs) -> PathBuf` —
  `rel_prefix.join(abs.strip_prefix(source_abs)).clean()`.
- `clean_new_name(name: &str) -> PathBuf` — `strip_leading_up(&Path::new(name).clean()).0`
  (normalize + drop any leading `..`/`/`/`.`).
- `strip_leading_up` stays as-is (already the `..`-clamp primitive).

### `populate_new_names`
- abs branch: `apply_renames(..., |abs| abs_rel_path(abs))`.
- rel branch: per source, `let prefix = clamped_rel_prefix(&source.original_path);`
  then `apply_renames(..., |abs| rel_rel_path(&source_abs, &prefix, abs))`.
- `apply_renames` unchanged: transform then `strip_relative_member` (GNU order).

### `populate_out_tree_rel`
- Replace the inline `extraction_base` derivation with
  `let rel_prefix = clamped_rel_prefix(&source.original_path);` and pass
  `Some((&source.abs_path, &rel_prefix))` to `build_new_out_tree_rows`.

### `catalog_to_target_abs` (regular branch)
- `None` → `root.join(abs_rel_path(catalog_path))`.
- `Some((source_abs, rel_prefix))` → `root.join(rel_rel_path(source_abs, rel_prefix, catalog_path))`.
  (The `relative_component` now carries the **relative** prefix, not the absolute base.)

### `build_new_out_tree_rows`
- new-name branch: `root.join(clean_new_name(r.new_name ...))` (drop the old `base`);
  re-clean + strip_leading_up in **all** new-name cases per the contract above.
- regular branch: unchanged (via `catalog_to_target_abs`).

## Tests
- `strip_leading_up`: `"../../a/b"→("a/b",2)`, `"../x"→("x",1)`, `"./a"→("a",0)`,
  `"/a/b"→("a/b",0)`.
- `clamped_rel_prefix`: `"../../Movies"→Movies`, `"/home/u/Movies"→home/u/Movies`,
  `"/"→""`, `"./a"→a`.
- `clean_new_name`: `"Movies/x"→Movies/x`, `"../Movies/x"→Movies/x`,
  `"../../.."→""`, `"/abs"→abs`.
- `rel_rel_path` / `abs_rel_path`: `(src=/home/u/Movies, prefix=Movies, abs=…/Best.mp4)`
  → `Movies/Best.mp4`; abs `/a/b` → `a/b`.
- Order lock: a transform anchored on the source prefix (e.g. `s,^Movies,Pics,`)
  applied to `rel_rel_path(...)` output matches (proves mangling sees the full path).

## Verification
- `cargo build -p tar-dedup`
- `cargo test -p tar-dedup --lib` (454 passed + the pre-existing
  `clear_archive_meta_drops_only_byte_counters` blocker).

## Implementation notes
- **Extra fix:** the earlier `kept.is_empty()` removal in `strip_relative_member` had
  accidentally dropped the `[strip as usize..]` slice too, so component stripping was a
  no-op (the existing strip tests caught it). Restored the slice, keeping the dead-check
  removal.
- `strip_leading_up` asserts its input is already clean (no `.`); the `clamped_rel_prefix`
  helper does the `clean()` step before delegating.
