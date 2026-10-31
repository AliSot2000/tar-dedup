# Plan: footer content-sha1 (finished-archive integrity)

Status: **agreed (2026-10-17), implementation round.** Adds a sha1 of the finished
compressed archive to the seekable footer, with a progress bar on the write side
and a `--force`-downgradable verification at first footer install during extract.

## New footer layout

```
[ compressed(tar) | MAGIC | sha1(tar[0..offset)) 20B | xz(sqlite) | sha1(xz) 20B | MAGIC | u64 offset ]
```

Both digests are sha1 (reuse `sha1 = "0.10"`, no new dependency). `offset` still
points at the first `MAGIC`; the content digest is the 20 bytes after it. No
production archives exist, so old-layout footers being rejected is acceptable
(noted in the module doc).

## Changes

1. **`write_footer(archive_path, sqlite_path, bar: Option<&ProgressBar>)`**
   (archive_footer.rs): after computing `offset`, stream-hash `archive[0..offset)`
   with `Sha1` chunked at `ARCHIVE_IO_BUF_SIZE`, feeding `bar`
   (`set_length(offset)` + `inc(chunk)`), then append
   `MAGIC | content_sha1 | xz | sha1(xz) | MAGIC | offset`.

2. **tar_writer stage** (`archive/tar_builder.rs::end_session`, where
   `write_footer` is called): push the hashing bar on top of the two below (the
   phase bar + global bar) via `push_sub_bar("archive integrity: ", BarKind::Bytes)`
   and pass `Some(&bar)`. Torn down with the phase bars at run end.

3. **`read_footer`** (archive_footer.rs): `footer_fixed_overhead()` gains one
   `SHA1_LEN` (`MAGIC + 2×SHA1 + MAGIC + OFFSET`); after the leading-MAGIC check,
   skip the 20-byte content digest; `xz_len` accounts for it. Presence gate and
   `has_valid_footer` stay structure-only (cheap).

4. **`verify_archive_integrity(archive_path) -> Result<bool>`** (archive_footer.rs):
   parse the footer (trailing offset + content digest after the leading MAGIC),
   hash `archive[0..offset)` chunked, compare. `Ok(true)` intact, `Ok(false)`
   mismatch, `Err` structural/I-O.

5. **Extract gate** (unarchive/scan.rs, only when `footer_this_pass`): call
   `verify_archive_integrity`; `Ok(false)`/`Err` → record a session error (path
   attached); `--force` → `tracing::warn!` + continue, else abort (`Err`).

## Verify

- `nix develop -c cargo build -p tar-dedup` and `--workspace`
- `nix develop -c cargo test -p tar-dedup --lib` (existing suite; footer format
  change only affects archives written by this build).
