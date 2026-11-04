# tar-dedup

A `tar` "wrapper" that **deduplicates a file tree before archiving**, for
efficient cold storage of unstructured data. It supports resumable
archive/extract runs, sparse-file materialization, and per-file metadata
capture (times, uid/gid, mode, xattrs, ACLs, SELinux, symlink targets), and is
built on top of the standard `.tar` container.

## Install

```
cargo install tar-dedup
```

Or grab a per-platform tarball / install script / Homebrew formula from a
[GitHub Release](https://github.com/AliSot2000/tar-dedup/releases), or use your
distribution's package manager once ported.

## Quickstart

```
tar-dedup archive -f backup.tar.xz -i ~/Documents   # snapshot (dedup + xz)
tar-dedup extract  -f backup.tar.xz                 # restore it
```

See the repository for the full manual and the `tar-dedup-lib` crate for the
underlying library.