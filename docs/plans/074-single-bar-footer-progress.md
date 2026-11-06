# 074 — Single-bar footer progress (register/unregister global)

Status: agreed + implemented.

## Problem

During the archive footer hash the UI draws three redundant bars: the `archive
integrity` hash sub-bar, a phase bar already at 100% (or a bogus `0B/0B` on the
resume path), and the global percent bar. The user wants the footer hash to be
the **single visible bar**.

## Design: narrow `register_global` / `unregister_global` interface

`progress.rs` gains a private presence flag `global_registered: Mutex<bool>`
(init `true` in `new`, which already `mp.add(global)`). Two public idempotent
methods own the flag together with the mp call:

- `register_global()` → `mp.add(global)` + set flag when it was unset.
- `unregister_global()` → `mp.remove(global)` + clear flag when it was set.

The two `insert_before(&self.global, …)` anchor sites (`begin_phase`,
`thread_bar` fallback) assert the flag before inserting. If anything runs a new
phase on the same `ProgressBarSet` after the footer removed the global, it
panics with a clear message instead of indicatif's bare `index().unwrap()`.
(`index()` is `pub(crate)` in indicatif, so we can't probe membership any other
way; the flag is the honest sentinel.)

`finish()` / `abandon()` / `inc_global` / `inc_both` don't anchor on the index —
they no-op safely on a removed handle — so they are not asserted.

## Footer entry point

`ProgressBarSet::single_bar()` = drop the phase bar (take, `reset_bar` Finish,
`mp.remove`) then `unregister_global()`. Both footer call sites in
`tar_builder.rs` then do:

```rust
rt.progress.single_bar();
let hash_bar = rt.progress.push_sub_bar("archive integrity: ", BarKind::Bytes);
match archive_footer::write_footer(..., rt.shutdown, Some(&hash_bar)) { ... }
```

No restore — the footer is terminal: after it, no `begin_phase` runs on the same
set (the `Done` phase exits the loop before entering; interrupts return; a resume
builds a fresh `ProgressBarSet`). The post-archive integrity battery
(`verify_after_phase`) uses `push_sub_bar`, which does not anchor on the global.

## Files
- `crates/tar-dedup-lib/src/progress.rs` (flag, register/unregister, single_bar, anchors)
- `crates/tar-dedup-lib/src/archive/tar_builder.rs` (two footer sites)

## Tests
- `register_unregister_global_toggles_flag_and_begin_phase_panics_while_unregistered`
- `single_bar_removes_phase_and_global_leaving_one_bar` (+ `finish()` no-ops safely)

## Verification
Local `cargo test -p tar-dedup-lib --lib`, then VM `--features test-all`.