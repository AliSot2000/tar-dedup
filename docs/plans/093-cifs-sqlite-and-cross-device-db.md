# 093 — SQLite on CIFS/SMB + cross-device retained DB

Status: **implemented (2026-11-15).**

## Symptom

Archiving directly onto an SMB/CIFS share failed before the first phase:

```
Error: Database(SqliteFailure(Error { code: DatabaseBusy, extended_code: 5 },
       Some("database is locked")))
```

The work DB file was created but stayed 0 bytes; the error came from
`Database::open` (`schema::initialize`'s `PRAGMA journal_mode=WAL`), not from a
double-open.

## Root cause

`strace` showed SQLite's default Unix VFS taking a shared byte-range lock and
then failing to upgrade it to the exclusive lock WAL requires:

```
fcntl(fd, F_SETLK, {l_type=F_WRLCK, l_start=1073741826, l_len=510}) = -1 EACCES
```

The CIFS server rejects the read→write upgrade of its own client's lock, which
SQLite reports as `database is locked`. Low-offset locks, `flock`, and
`unix-dotfile`/`unix-none`/`unix-excl` all work on the same mount; only the
default VFS's WAL upgrade fails.

## Fix

`Database::open` now retries with the `unix-excl` VFS when:

- the first `open + schema::initialize` fails with `DatabaseBusy` /
  `DatabaseLocked`, and
- the DB's parent directory is on CIFS/SMB (`statfs` magic: CIFS, SMB2, SMB).

`unix-excl` takes the exclusive lock directly (no read→write upgrade) and
supports WAL, including a heap-simulated shm. Non-SMB filesystems keep the
default VFS, so genuine local lock contention is not silently bypassed.

A warning is logged once per affected open with the reason and VFS switch.

## Related fix: retained DB across filesystems

With a local `--work-dir` and the archive on the NAS, cleanup's kept-DB rename
failed with `EXDEV`:

```
rename(...) = EXDEV "Invalid cross-device link"
```

`cleanup_workdir` now uses `move_db_file`:

1. `rename` (fast path).
2. `InvalidInput` → retry with `:` replaced by `-` (NTFS-friendly name).
3. `CrossesDevices` → `copy_file_reflink` + unlink source; if the copy fails,
   retry once with the sanitized name.

The copy path also covers a local work dir with a remote archive/extraction
root.

## Remaining CIFS limitation (not fixed)

CIFS with `nounix` rejects `symlink(2)` (`EOPNOTSUPP`), which the archive stage
phase requires (stage symlinks point at the original payloads). The supported
layout for network storage is therefore:

```
tar-dedup archive -f /nas/backup.tdd.tar.xz -i /nas/src --work-dir /local/work ...
```

The work DB and stage symlinks stay local; only the finished archive lives on
the share. Extract can run with its work dir on the share (scan copies regular
files), though cleanup may warn that the `.estage` directory is not empty when
open SQLite sidecars are still held.

## Tests / verification

- `cargo test -p tar-dedup-lib --lib`: 776 passed.
- New `move_db_file_crosses_devices_by_copying` (Linux, skips when
  `/tmp` and `/dev/shm` are the same device).
- Manual on the CIFS mount:
  - small archive with work dir on the share now completes the DB phases (hits
    the documented symlink limit at stage);
  - archive with `--work-dir /tmp/...` and `--db-retention always` completes and
    copies the retained DB to the share (EXDEV path);
  - archive → extract round trip with the extract work dir on the share
    completes.

## Docs

`AGENTS.md` gotchas updated with the CIFS/SMB behavior and the `--work-dir`
recommendation.
