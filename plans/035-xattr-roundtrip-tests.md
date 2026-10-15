# Plan: xattr / POSIX-ACL / SELinux round-trip + malformed test battery

Status: **agreed (2026-10-13), implementation round.**

Scope: unit tests only, in `common/xattr.rs` (replaces the `// TODO!!!` at the bottom),
plus exactly one tiny production-code touch: a `perm <= 7` guard in `set_file_acl` so a
malformed permission returns `Err` instead of panicking inside the `posix-acl` crate
(`check_return` is `assert_eq!` → `acl_add_perm(EINVAL)` on `perm > 7` panics).

## Environment / gating

- Tests live inline in `#[cfg(test)] mod tests` (repo convention; inline tests get private
  access to `XattrDump`/`Encoding` for order-independent JSON comparison).
- **Probe-based auto-skip**: each test first performs a trivial write on a probe file; on
  `ENOTSUP`/`EOPNOTSUPP`/`EPERM` it prints `skipped (unsupported): …` and returns. Keeps
  `cargo test -p tar-dedup --lib` green on a plain dev box and auto-runs on the VM.
- **`TAR_DEDUP_TEST_DIR` env override**: if set, tests create files there; otherwise
  `tempfile::tempdir()`. SELinux needs a labelled fs — `/tmp` (default tempdir) is often
  tmpfs, which cannot hold `security.selinux` labels.
- Root/SELinux-only tests still probe-skip (no `#[ignore]`), so the same command works
  everywhere.

## Part 1 — exhaustive per-type round trips

`capture(f1)` → `restore(f2, capture)` → `recapture(f2)`; assert `capture == recapture`.

- **xattr** (`symlink_set_xattr(path, name_without_user_prefix, &[u8])`; writes `user.<name>`):
  - `xattr_round_trip_single_utf8`
  - `xattr_round_trip_many_keys_and_binary_values` — several keys at once, incl. non-UTF-8
    byte values, an empty value, and an empty set
  - `xattr_round_trip_both_encodings` — hand-crafted `XattrDump` JSON with
    `"encoding":"base64"` and `"encoding":"utf8"` to exercise both decode branches of
    `set_file_xattrs`
- **ACL** (`PosixACL::empty()` + `.set(Qualifier::…, perm)` + `.write_acl(path)`; mask set
  to the group-class union so `write_acl`'s `fix_mask()` is a no-op → round trip stable):
  - `acl_round_trip_minimal_mode` — user-obj / group-obj / other (+ mask)
  - `acl_round_trip_all_entry_types` — every qualifier kind on one ACL
  - `acl_round_trip_repeated_named_users` — several `user:UID` entries
  - `acl_round_trip_repeated_named_groups` — several `group:GID` entries
  - `acl_round_trip_perm_combinations` — perms 0..7 combos
  - compare parsed `HashMap<String,u32>` (order-independent)
- **SELinux** (one context per inode; no repetition possible — matrix is context shape):
  - `selinux_round_trip_base` — `user:role:type`
  - `selinux_round_trip_mls_level` — `…:s0`
  - `selinux_round_trip_categories` — `…:s0:c0.c1023`
  - `selinux_round_trip_low_high_range` — `…:s0-s15:c0.c1023`
  - byte equality `capture == recapture` (no parsing)

## Part 2 — everything at once

- `all_attributes_round_trip_on_single_file` — one file carrying the full Part-1 set
  simultaneously: full ACL (all entry types with repeated named users/groups), many xattrs
  incl. binary, full MLS+categories SELinux context; capture→restore→recapture all three.

## Malformed (parser-level runs anywhere; write-level gated by probe)

- **SELinux**:
  - `selinux_malformed_context_loop` — `b"not-a-valid-context"`, too-few/too-many fields →
    each `set_file_selinux_data` returns `Err(SELinux)`
  - `selinux_malformed_interior_nul` — `b"u:r:t\0s0"` → `Err(Io)` from `CString::new`
    (runs anywhere, no SELinux needed)
  - `selinux_malformed_empty` — `b""` → Err
- **ACL**:
  - `acl_malformed_bad_uid` — `"user:abc":7` → parser Err
  - `acl_malformed_unknown_qualifier` — `"bogus":7` → parser Err
  - `acl_malformed_not_a_map` — `"[1,2]"` → Json Err
  - `acl_malformed_missing_mandatory` — only `"user:1000":7`, no user-obj/group-obj/other/
    mask → `Err(PosixAcl)` from `write_acl` validation
  - `acl_malformed_repetition` — several malformed entries in one JSON → Err
  - `acl_malformed_perm_out_of_range` — `"user-obj":8` → `Err(General)` via the new guard

## Production-code change

`set_file_acl` (common/xattr.rs): before `new_acls.set(qualifier, value)`, reject
`value & !0b111 != 0` with `Err(FileStatError::General { path: Some(path.to_path_buf()),
message: format!("invalid ACL permission {value}") })`. Prevents the upstream crate panic.

## Findings recorded (no code change)

1. SELinux get reads translated (`of_path(path, false, false)` → `lgetfilecon`) but set
   writes raw (`from_c_str(..., true)` + `set_for_path(path, false, false)` →
   `lsetfilecon_raw`). capture→restore→recapture is self-consistent; revisit raw-vs-translated
   later.
2. `get_file_xattr` silently skips (println + continue) xattr keys that fail ZString
   conversion (non-UTF-8 names) rather than erroring.

## Build / test

- `nix develop -c cargo test -p tar-dedup --lib` (dev profile only)
- `TAR_DEDUP_TEST_DIR=<labelled-dir>` on the VM to enable the SELinux battery.
