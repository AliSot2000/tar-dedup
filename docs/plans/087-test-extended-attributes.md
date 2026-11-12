# 087 — Gate extended-attribute round-trip tests behind `test-extended-attributes`

Status: **implemented (2026-11-12).** `test-all = ["test-compression",
"test-extended-attributes"]`; feature-off `--lib` = 764 (18 gated tests absent),
feature-on host = loud SELinux failure (no labels on NixOS), VM `test-all` green.

## Motivation

The xattr / POSIX ACL / SELinux round-trip tests run in the default suite and
start with a runtime capability probe (`probe_xattr` / `probe_acl` /
`probe_selinux` in `common/xattr.rs`, or an inline
`symlink_set_xattr(...).is_err()` / `get_file_acl()` / `get_file_selinux_data()`
in `archive/inventory.rs`). When the filesystem lacks the capability the test
prints `skipped (unsupported): …` and `return`s — a **silent pass**. On a host
whose filesystem supports only some of these (e.g. the author's repo lives on
ntfs3: xattrs work, POSIX ACLs and SELinux do not), the suite reports green
without ever exercising those paths. `docs/manual.md:749` even documents the
requirement: *"a file system with SELinux, posix_acl and xattr capabilities is
needed."*

This plan gates those tests behind a feature `test-extended-attributes` (the
`test-compression` pattern) and, when enabled, turns a missing capability into a
hard failure with the message `file system does not support <feature>` instead
of a skip.

## Rules

1. **New feature** `test-extended-attributes = []` on `tar-dedup-lib`.
2. **`test-all` includes it** (`test-all = ["test-compression",
   "test-extended-attributes"]`). Consequence (agreed with the user):
   `cargo test --features test-all` on a filesystem without xattr+ACL+SELinux
   support now **fails** instead of silently passing; full `test-all` runs on
   the VM (btrfs + SELinux). The default `cargo test -p tar-dedup-lib --lib`
   (no features) drops the gated tests entirely and stays green everywhere.
3. **Gated tests fail loudly on unsupported filesystems:** a failed probe
   panics with `file system does not support <kind>: <os error>`, never
   `eprintln!("skipped…"); return;`.
4. **Feature-off default suite keeps** the pure error-path tests that need no
   filesystem support (they validate parsing/validation in-memory before any
   fs write).

## Gated tests (18)

### `common/xattr.rs` (`tests` module) — 12

- xattr round trips: `xattr_round_trip_single_utf8`,
  `xattr_round_trip_many_keys_and_binary_values`, `xattr_round_trip_both_encodings`
- ACL round trips: `acl_round_trip_minimal_mode`,
  `acl_round_trip_all_entry_types`, `acl_round_trip_repeated_named_users`,
  `acl_round_trip_repeated_named_groups`, `acl_round_trip_perm_combinations`
- SELinux: `selinux_round_trip_all_context_shapes`,
  `selinux_malformed_empty`, `selinux_malformed_context_loop`
- Combined: `all_attributes_round_trip_on_single_file` — the SELinux step
  changes from best-effort to a **hard requirement** (agreed with the user):
  probe `SELinux` up front and panic if unsupported; remove the trailing
  best-effort `match` that turned a capture error into `None`.

Also gated (helpers only used by the gated tests, avoids dead-code in
feature-off builds): `probe_xattr`, `probe_acl`, `probe_selinux`,
`write_user_xattr`, `write_acl`, `parse_xattr_data`, `parse_acl_map`,
`selinux_context_variants`, `assert_selinux_round_trip`.

New helper (feature-gated alongside them):

```rust
fn require_fs(kind: &str, probe: Result<(), String>) {
    if let Err(e) = probe {
        panic!("file system does not support {kind}: {e}");
    }
}
```

**Stay ungated:** `ctx`, `decoded_xattr_map` (imported by ungated
permissions tests), and `selinux_malformed_interior_nul`,
`acl_malformed_bad_uid`, `acl_malformed_unknown_qualifier`,
`acl_malformed_not_a_map`, `acl_malformed_missing_mandatory`,
`acl_malformed_repetition`, `acl_malformed_perm_out_of_range`.

### `archive/inventory.rs` — 3

- `entry_metadata_gating_xattr`, `entry_metadata_gating_acl`,
  `entry_metadata_gating_selinux`: gate with the feature; replace the inline
  `if … .is_err() { eprintln!("skipped (unsupported): …"); return; }` with a
  panic `file system does not support <user xattrs|POSIX ACLs|SELinux>: {e}`.

### `unarchive/permissions.rs` — 3

- `apply_one_xattr_applies_when_supported`,
  `apply_one_acl_applies_when_supported`,
  `apply_one_selinux_applies_when_supported`: gate with the feature; replace
  `if let Err(e) = probe_X(&scratch) { eprintln!(…); return; }` with a
  `require_fs`-style panic.
- Split the `:579` import: keep `decoded_xattr_map` ungated; gate the
  `use crate::common::xattr::tests::{probe_acl, probe_selinux, probe_xattr};`
  import with the same feature.
- `apply_one_no_xattrs_gate_skips`, `apply_one_no_acls_gate_skips`,
  `apply_one_no_selinux_gate_skips` stay ungated.

## Edit map

| File | Change |
|------|--------|
| `crates/tar-dedup-lib/Cargo.toml` | add `test-extended-attributes = []`; `test-all = ["test-compression", "test-extended-attributes"]` |
| `src/common/xattr.rs` | add `require_fs`; gate helpers + 12 tests; replace skips with `require_fs(...)`; hard-require SELinux in the combined test |
| `src/archive/inventory.rs` | gate 3 `entry_metadata_gating_*`; inline skip → panic |
| `src/unarchive/permissions.rs` | split import (line 579); gate 3 `apply_one_*_applies_when_supported`; skip → panic |
| `AGENTS.md` | document the new feature; note `test-all` now requires a capable filesystem (full suite on the VM) |
| `docs/manual.md:749` | mention the feature gate / that extended tests require xattr+ACL+SELinux fs |

## Verification

- Default (feature-off): `cargo test -p tar-dedup-lib --lib` → the 18 gated
  tests are absent; the rest pass on host and VM.
- Feature-on, unsupported fs (author's ntfs3 host): `cargo test -p tar-dedup-lib
  --lib --features test-extended-attributes` → xattr tests pass, ACL/SELinux
  tests **fail** with `file system does not support <…>` (expected).
- `cargo test --features test-all` on host → **fails** on ACL/SELinux (by
  design per the agreed rule); on the VM (btrfs+SELinux, synced via tar-over-ssh,
  `TMPDIR=/home/test-user/tmp`) → all green.
