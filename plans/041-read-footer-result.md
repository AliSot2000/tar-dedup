# Plan: read_footer -> Result<bool> (footer presence tri-state)

Status: **agreed (2026-10-17), implementation round.** Reworks `read_footer` to
distinguish "footer extracted" / "no footer" / "footer corrupt or I/O error",
instead of the current all-error (and swallowed in scan) design.

## Signature

`pub fn read_footer(archive_path: &Path, dest: &Path) -> Result<bool>`

- `Ok(true)` — a valid footer was present and extracted to `dest`.
- `Ok(false)` — no footer present.
- `Err(_)` — a footer structure is present but failed to extract (corrupt
  magic/sha1/xz, decompress, or an I/O error).

## Presence gate (per the trailing-MAGIC correction)

Layout: `[ tar | MAGIC | xz(-9e) | sha1(xz) | MAGIC | u64 offset ]`.

A footer is **present** iff the trailing `MAGIC` (the one right before the `u64`
offset, at `len - OFFSET_LEN - MAGIC_LEN`) matches AND the offset it stores
resolves to a valid position inside the file (`offset + min_len <= len`, no
overflow). Both are cheap, so a plain tar without a footer lands on `Ok(false)`
instead of a bogus error.

After the presence gate, any failure is a real error: leading-MAGIC mismatch,
`xz`-length underflow, sha1 mismatch, decompress failure, or dest write failure.

## Call sites

- `unarchive/scan.rs`: replace `opt_db.is_ok()`; on `Err` record a session error
  (existing `record_session_error` helper, path attached) and treat the pass as
  footer-less; `footer_this_pass = !resume_db && present`.
- `archive_footer.rs::has_valid_footer`: `matches!(read_footer(..), Ok(true))`.
- `archive_footer.rs::extract_footer_db`: return `Result<Option<PathBuf>>`
  (`None` when absent); currently no callers, adapted to compile.
- `cmd/inspect.rs`: `Ok(true)` → open footer DB; `Ok(false)` → fall through to
  the leading-manifest peek; `Err` → keep the existing `footer_err` note.

## Verify

- `nix develop -c cargo build -p tar-dedup`
- `nix develop -c cargo test -p tar-dedup --lib` (behavior unchanged; `db::scan`
  + `unarchive::scan` + inspect unaffected).
