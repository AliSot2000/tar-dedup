# 065 — `check` command: footer integrity verification

Status: approved in conversation (build).

## Goal

A small `tar-dedup check` subcommand that verifies an archive's integrality
from its seekable sqlite footer, if present:

```
[ compressed(tar archive) | MAGIC | sha1(archive content) | xz(-9e) of final-database.sqlite | sha1(xz blob) | MAGIC | u64 offset ]
```

It hashes (a) the finished compressed archive `[0..offset)` against the stored
content digest, and (b) the stored xz blob (the compressed catalog) against the
stored xz digest. **No decompression** — only the two sha1s are computed. One
progress bar spans both passes.

## CLI

```
tar-dedup check -f <ARCHIVE> [-q|--quiet]
```

- `CheckArgs { -f archive (required; resolved against cwd), -q/--quiet }`.
- Default: single progress bar + one verdict line.
- `--quiet`: no bar, no message — exit code only.

## Core (`archive_footer.rs`)

```rust
pub enum IntegrityVerdict {
    Ok,
    NoFooter,
    InvalidFooter,
    ContentMismatch,
    DatabaseMismatch,
    BothMismatch,
}

pub fn verify_footer(path: &Path, bar: Option<&ProgressBar>) -> Result<IntegrityVerdict>
```

- `Err` = generic filesystem error (open/metadata/seek/read).
- Reuses `locate_footer` (`Ok(None)` → NoFooter; leading-MAGIC mismatch →
  InvalidFooter; `offset` otherwise).
- Reads the stored content digest (right after the leading MAGIC) and the
  stored xz digest.
- Pass 1: `hash_archive_bytes(path, offset, bar)` — bar length = `offset`,
  message "hashing archive content".
- Pass 2: hash the xz blob
  (`xz_len = len - (offset + footer_fixed_overhead())`) with the bar continuing
  (`total = offset + xz_len`), message "hashing database".
- Compare both; combine into the verdict.

Existing `verify_archive_integrity`/`read_footer`/`has_valid_footer` untouched.

## Exit codes

| Verdict                                    | Exit |
|--------------------------------------------|------|
| OK (both hashes match)                     | 0    |
| generic filesystem error                   | 1    |
| No footer                                  | 2    |
| Invalid Footer format                      | 3    |
| Hash mismatch of Content                   | 4    |
| Hash mismatch of Database                  | 5    |
| Hash mismatch of Content and Database      | 6    |

## Wiring

- `cli.rs`: `Command::Check(CheckArgs)`.
- `main.rs`: `Command::Check(args) => tar_dedup_lib::cmd::check::run(&args)`.
- `cmd/mod.rs`: `pub mod check;`; `cmd/check.rs::run` builds the bar (unless
  quiet), calls `verify_footer`, prints the verdict (unless quiet), and exits
  with the mapped code.

## Tests (`archive_footer` module)

One per verdict using the existing fixtures:

- `Ok` on `build_footer_archive`.
- `NoFooter` on `build_archive`.
- `InvalidFooter` by corrupting the leading MAGIC at `footer_offset`.
- `ContentMismatch` by corrupting a byte in `[0, offset)`.
- `DatabaseMismatch` by corrupting a byte inside the xz blob
  (`offset + MAGIC + sha1_len`).
- `BothMismatch` by corrupting both regions.

## Readme

`scripts/gen_readme.py` regenerates the option tables → adds the `### check`
section (plan 064 machinery); run with `--check` afterwards.

## Implementation order

1. `archive_footer.rs` (`IntegrityVerdict` + `verify_footer`).
2. CLI (`CheckArgs`, enum arm, dispatch, `cmd/check.rs`).
3. Tests.
4. Readme regeneration.
5. Build, `cargo test -p tar-dedup-lib --lib`, and an end-to-end `check` on a
   produced archive (host; VM on request).