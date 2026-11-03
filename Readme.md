# Tar-Dedup

A spiritual `tar` "wrapper" that **deduplicates a file tree before archiving**, for efficient
cold storage of unstructured data. It supports resumable archive/extract runs,
sparse-file materialization, and per-file metadata capture (times, uid/gid, mode,
xattrs, ACLs, SELinux, symlink targets) and is built on top of the standard .tar container and is backwards compatible 
with `tar`.   

## Quickstart

```sh
cargo build -p tar-dedup --release

# Snapshot a tree (dedup + xz compression, resumable)
tar-dedup archive -f backup.tar.xz -i ~/Documents

# Restore it
tar-dedup extract -f backup.tar.xz

# Verify an archive's integrity (footer catalog + content hashes)
tar-dedup check -f backup.tar.xz
```

## Documentation

The **manual** lives in [`docs/manual.md`](docs/manual.md). Jump straight to a section:

- [Usage & subcommands](docs/manual.md#usage) — [archive](docs/manual.md#archive) ·
  [extract](docs/manual.md#extract) · [resume](docs/manual.md#resume) ·
  [inspect](docs/manual.md#inspect) · [check](docs/manual.md#check)
- [Motivation / design choices](docs/manual.md#motivation)
- [Disclaimers](docs/manual.md#disclaimers) (unstable, divergence from `tar`)
- [Development](docs/manual.md#development) — archive layout, canonical files

API reference: `cargo doc -p tar-dedup-lib --open`.

The option tables in the manual are generated from the compiled CLI by
`scripts/gen_readme.py` (single source of truth: clap metadata). See the script
header and `scripts/deprecated.json` for details.

## Status

Pre-1.0, unstable software. Archives created before **v1.0.0** have no
compatibility guarantees — see [Disclaimers](docs/manual.md#disclaimers).