# Plan: VM integration-test suite (CLI end-to-end)

Status: agreed 2026-10-29. The first real-hosts integration pass, driven over SSH
from the host into the disposable Fedora 44 VM (`test-user@192.168.56.101`).
Targets goal (a) integration testing and (b) extended-attribute correctness.

## Why a shell harness

- Integration needs the **built CLI** against a **real filesystem** (btrfs/tmpfs,
  SELinux, real xattrs) — `cargo test` lib units can't exercise the binaries
  `cmd/*` paths or the actual `rm -rf`-able sandbox.
- A single `integration/run.sh` runs on the VM (or any Linux) with only `bash`,
  coreutils, `attr` (getfattr/setfattr), and the untarred repo. No cargo needed
  at runtime — it expects `target/release/tar-dedup`.
- Assertions are explicit, greppable, and exit nonzero on regression (no hidden
  `|| true`). Each scenario cleans its own scratch dir under `$SCRATCH`.

## Layout

`integration/run.sh [scenario...]` — no args runs every scenario (SHA-ordered by
name). Helpers: `make_tree`, `assert_file_eq`, `assert_xattr_eq`, `assert_absent`,
`archive`, `extract`. Scenario files kept in `integration/scenarios/*.sh` and
`source`d (each is a bash function; run.sh iterates the registered names).

## Scenarios (phase 1 — xattr correctness + resume)

1. `xattr_user` — `user.*` round-trip on regular files: archive with capture on,
   extract with `--xattrs`, assert values via `getfattr --only-values`.
2. `xattr_on_symlink` — xattr on the symlink itself (`setfattr -h`), namespaced
   round-trip through l*xattr capture/restore.
3. `xattr_binary` — non-UTF-8 value (`printf '\xff\xfe'`); round-trip must be
   byte-identical.
4. `xattr_none_default` — extract **without** `--xattrs`/`--apply-all-metadata`
   leaves files xattr-less (default "apply nothing").
5. `xattr_apply_all` — `--apply-all-metadata` restores every bit (xattrs + mode +
   mtime) in one flag.
6. `dedup_roundtrip` — duplicates collapse to one canonical; both members extract
   with their own xattrs and byte-equal content.
7. `resume_archive` — `SIGINT` mid-hash (or `--jobs 1` + a large tree), re-run
   `resume --work-dir`, archive completes with a valid footer and identical bytes.
8. `path_clamp` — `--transform` injecting `../` stays under the extraction root
   (from `integration-testing-ideas.md`).

## Phase 2 (after the above is green)

- `--no-strict-separation` fan-out, source-overlap hardlink election order,
  `ensure_parent` via `--files-from`, `--link-tree` (canonical_id stays NULL),
  same-owner/SELinux/mknod **gated behind root** (skip without it).

## VM notes / gotchas

- SSH from host: `ssh -F /tmp/vm-ssh/config tardedup-vm` (IdentityAgent none —
  a local bitwarden agent socket stalls the handshake otherwise).
- Repo sync: `tar czf - … | ssh … 'tar xzf -'` (host `rsync` loads a broken
  libacl); exclude `target/`, `.git`, scratch.
- `--release` was broken (`validate_materialize_result` gated by
  `cfg(debug_assertions)` but called under runtime `cfg!`) — fixed in
  `db/place.rs`; keep release builds green.
- Extract applies nothing by default: xattr/mode/times need `--xattrs` /
  `--apply-all-metadata`. That is by design; the suite asserts both ways.
- The `--release` binary must be rebuilt after sync (rustup stable 1.99.1 on VM,
  vendor-independent).

## Implementation notes (2026-10-29) — phase 1 DONE

Harness: `integration/run.sh [scenario...]`, scenarios in
`integration/scenarios/*.sh` (bash + coreutils + `attr` only). Scratch defaults
to `~/.cache` (the Fedora `/tmp` tmpfs is ~2 GiB and churns). Full suite:
**7 passed / 0 failed / 1 skipped** on the VM (Fedora 44, btrfs, SELinux) and on
the host (NixOS) — parity.

### Bugs found by this suite (all fixed in this round)
1. **Release builds were broken**: `db/place.rs` called
   `validate_materialize_result` (a `#[cfg(debug_assertions)]` fn) under a
   runtime `cfg!(debug_assertions)` — release binary failed to compile.
   Call site is now compile-time gated.
2. **SELinux context stored with trailing NUL**: the `selinux` crate's
   `as_bytes()` includes the NUL when `size` is set (as `of_path` does).
   `set_file_selinux_data()` then failed `CString::new` with
   "nul byte in provided data" → every metadata restore on an SELinux host
   failed. Both capture (trim) and restore (trim) in `common/xattr.rs`.
3. **Infinite metadata loop on directories**: `list_out_tree_for_permissions_dirs`
   (unlike the non-dir lister) did not exclude `ErrorWhileApplyingMetadata` rows,
   so a persistently failing dir was re-pulled every batch → unbounded error
   spam (12k errors before a time-out SIGTERM). Added the `:error` filter.
4. **Dedup content-equal / different-inode members dropped**: the out-tree
   hardlink election left non-elected regular files with `canonical_id = NULL`,
   which neither the materialize queue nor the hardlink pass picks up → the
   second identical-content file was silently missing after extract.
   `mark_global_canonical` / `mark_source_canonical` now make such rows
   self-canonical (step 3); the materialize pull already resolves the copy
   source through `files.canonical_id`, so both paths materialize.

### Scenario portability
- `xattr_on_symlink` is skipped where the fs rejects `user.*` on symlinks
  (btrfs → EPERM); tmpfs/ext4 allow it. Probe-and-skip via a `$RUN/.skipped`
  marker.
- `--apply-all-metadata` restores xattrs+mode+mtime (asserted).
- Extract applies nothing by default (asserted via `xattr_none_default`).

### VM/ops plumbing (kept out of the repo)
- SSH alias: `ssh -F /tmp/vm-ssh/config tardedup-vm` (must set `IdentityAgent
  none` — a dead local bitwarden agent socket stalls the handshake).
- Repo sync via `tar | ssh tar` (host `rsync` links the wrong libacl; breaks).
- VM toolchain: rustup stable 1.99.1; `dnf` (one-time, by owner):
  `gcc clang make pkgconf-pkg-config libselinux-devel libacl-devel
  libzstd-devel bzip2-devel` (+ `xz-devel zlib-devel` optional/absent).
