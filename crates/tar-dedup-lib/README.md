# tar-dedup-lib

Library backing the [`tar-dedup`](https://github.com/AliSot2000/tar-dedup)
command-line tool: a `tar` "wrapper" that **deduplicates a file tree before
archiving** for efficient cold storage of unstructured data.

It provides resumable archive/extract pipelines, sparse-file materialization,
and per-file metadata capture (times, uid/gid, mode, xattrs, ACLs, SELinux,
symlink targets), all driven by a work-directory SQLite state machine.

See the [project repository](https://github.com/AliSot2000/tar-dedup) and the
`tar-dedup` crate for the user-facing manual.