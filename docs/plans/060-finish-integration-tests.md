# Plan: finish the integration tests

Status: agreed 2026-10-30 (deliberately after the `MaterializeConflict` gap,
`058-materialize-conflict`, which is committed). Closes out `../archive/integration-testing-ideas.md`.

## A — New shell scenarios (10) → `../../integration/scenarios`

| Scenario | Idea | Assertion / notes |
|---|---|---|
| `paths_relative_input` | #4 | `-C $RUN/cwd -i ../src`; clamped target under `<ed>`, nothing escapes (`find`-prune incl. src) |
| `paths_strip_transform_order` | #6 | member anchored on first path component (path_clamp pattern); `--transform "s,^${first}/,x/x/,"` + `--strip-components 1` → `ed/x/…`; strip-first variant absent |
| `paths_empty_name_skip` | #7 | transform reducing whole member to `""` → file absent, siblings present (expr tuned on VM) |
| `paths_absolute_root` | #5 | absolute `-i`; target `== <ed> + ${src#/}`; (real `/` root skipped: would recurse fs) |
| `path_collision_transform` | — | two members → same target (`s,^f[12]…,merged.txt,`): first-wins content, second absent; identical under `--no-errors` |
| `no_strict_separation_fanout` | #1 | overlapping `-i` roots **with `ln` hardlinks**, `--no-strict-separation`; every file once, contents correct; hardlink detection ON |
| `link_tree` | #8 | `--link-tree`; outputs `-L` → `readlink` into `<ed>/.sources/`, `cat` reads through |
| `hardlink_grouping_none` | #10 | `ln` pair + normal file; `--hard-link-grouping none` → pairwise distinct `%i`, nlink 1 |
| `hardlink_global_vs_source` | #10 | cross-source `ln` pair; `global` → shared `%i`; `source` → split |
| `hardlink_overlap_matrix` | #10 | tree `b/{h, c/h, c/e, c/d/e}` pairs h↔c/h, c/e↔c/d/e; M1 `-i b -i b/c` grouped; M2 `-i b/c -i b` h-pair split (order effect); M3 `-i b/c/d -i b --no-recursion` depth-split/absent; `stat -c %i` |

All new scenarios: `--no-strict-separation` where overlapping; hardlink scenarios
**must not** pass `--no-hardlink-detection`.

## B — In-process lib unit tests (2 genuinely missing)
- `mark_source_canonical_overlap_order` (`db/place.rs`): sources A(`/a/b`),
  B(`/a/b/c`), `ln`-linked trio across b∪c; A-first → one group / B-first → c-group
  + b split; assert `out_tree.canonical_id`.
- `no_strict_separation_fanout_ref_out`: overlapping sources → shared out row
  accumulates `ref_out` rows for both sources (out-row dedup already covered by
  `same_file_fanout_stays_silent`).

(Resume-prologue idempotency already exists in `unarchive/place_prologue.rs`; the
collision unit tests already landed in `058-materialize-conflict`.)

## C — Cargo driver + docs
- `../../crates/tar-dedup-cli/tests/integration_shell.rs`: one `scenario_*` test per new
  scenario.
- `../archive/integration-testing-ideas.md`: status column for every item.

## D — Full suite — VM only (after tar-over-ssh sync)
1. `cargo test --workspace`
2. `cargo test -p tar-dedup --lib --features test-all`
3. `cargo test -p tar-dedup --doc`
4. `bash integration/run.sh` (all scenarios; `xattr_on_symlink` skipped on btrfs)
5. `cargo test -p tar-dedup-cli --test integration_shell`
6. optional `cargo llvm-cov … --summary-only`

Implementation order: A (easiest → hardlink group → overlap matrix) → B → C →
local verify → VM run.
## Implementation notes (2026-10-30) — DONE

- 10 new scenarios; 3 bugs/insights found while building them:
  - The **M2 order effect only exists under `--hard-link-grouping source`** —
    the default `global` grouping is order-independent (`mark_global_canonical`).
    The matrix scenarios therefore pass `--hard-link-grouping source`.
  - Empty-name transform needs **three** delimiters (`s,pat,,`); two leaves the
    replacement unterminated per the parser.
  - `paths_empty_name_skip` verified the sentinel end-to-end (file absent, sibling
    present).
- In-process tests: `mark_source_canonical_overlap_{covering_source_first,nested_source_first}`
  + `no_strict_separation_fanout_accumulates_ref_out` (via the batched inserter).
- VM-only discovery: `common::xattr::tests::selinux_round_trip_all_context_shapes`
  failed on a real SELinux host (policy refuses the `s0-s15` MLS-range variant for an
  unconfined user). Reworked to probe-set each variant and **skip policy-denied
  ones** (asserting ≥1 exercised), passing on both NixOS (SELinux-less) and Fedora.

## Verified — VM only (Fedora 44, btrfs, SELinux)
1. `cargo test --workspace` — sparse-cp 7/0 · tar-dedup lib **692/0** · cli_smoke 4/0 ·
   integration_shell **20/0** · doctests 1/0
2. `cargo test -p tar-dedup --lib --features test-all` — **717/0**
3. `cargo test -p tar-dedup --doc` — 1/0
4. `bash integration/run.sh` — **19 passed, 1 skipped** (`xattr_on_symlink`, btrfs)
5. `cargo test -p tar-dedup-cli --test integration_shell` — **20/0**

Host (NixOS) parity: same lib/workspace counts (692/0), `run.sh` 19/1 skip.
(Also: `on_sent_receives_only_handed_items` is a pre-existing flaky timing test —
passes in isolation and on rerun.)
