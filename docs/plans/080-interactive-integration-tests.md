# 080 — Interactive integration tests + DIR_RO / DIR_RW fixtures + CLI-error scenarios

Status: **approved — implementing (2026-11-07).**

## Motivation

`integration/run.sh` runs bash scenarios end-to-end and automatically. The author
wants to **also** drive the same scenarios *interactively*: watch and approve each
action (create files, hardlink, delete, set metadata, and each `tar-dedup`
invocation) before it runs, using real user-supplied directories, so the tool can
be exercised over genuine trees and validated manually. Also: cover the CLI-level
include/exclude validation errors (which should exit non-zero at the CLI, not deep
in the pipeline).

## Decided design points

1. **Two ways to run a scenario:**
   - **Automatic** (default, byte-identical to today): `integration/run.sh [scenario...]`.
   - **Manual** (interactive): `integration/run.sh --manual <scenario>` **or**
     `IT_MODE=manual integration/run.sh <scenario>` (both supported).
2. **Prompt scope:** both the FS-mutating setup commands AND each `tar-dedup`
   invocation (`ar`/`ex`/`bin`) are gated in manual mode.
3. **Scope of this change:** convert all 19 existing scenarios
   (`printf…>file → write_file`, inline `"$BIN"… → bin`), add the CLI include/exclude
   error scenarios and the `DIR_RO`/`DIR_RW` demo scenarios, plus a README.
4. **Prompt IO:** markers + the `[r]/[a]/[s]` prompt go to **`/dev/tty`** (read from
   the tty) so per-command `>/dev/null` redirects cannot hide them.

## Mechanism (validated)

Two candidate approaches were rejected empirically:

- **DEBUG-trap skip** (`return 2` from a `DEBUG` trap): a skipped command yields
  status 2, which aborts the `( set -e; ... )` subshell — the `s` branch is
  impossible under `set -e`. Rejected.
- **Plain wrapper functions over redirect writes**: `printf 'x' > file` opens /
  truncates the target *before* the function body runs, so `s` would destroy an
  author-made file. Rejected for redirect writes.

Adopted:

- **Wrapper functions** gate non-redirect mutating commands (`mkdir cp ln rm rmdir
  mv setfattr setfacl chmod chown touch truncate install mknod mkfifo`). On `s` the
  wrapper returns `0` without running → `set -e`-safe. Automatic mode calls through
  to `command "$@"` unchanged.
- **`write_file <path> <fmt> [args…]`** handles file content (enforced `printf "$fmt"
  "${@:3}" > "$path"`), so `s` never clobbers a hand-made file. Existing
  `printf 'CONTENT' > PATH` sites convert mechanically.
- **`bin <args…>`** is a gated `"$BIN" "$@"` runner; **`ar`/`ex`** become gated too,
  so every tool invocation prompts.

Prompt flow per gated action:

```
[tool]  $ tar-dedup archive -f /ea/run/src/out.astar -i /ea/run/src
[r]un / [a]bort / [s]elf > _
```

- `r` → run it.
- `s` (**self**) → skip; author already performed it by hand (e.g. on a
  `DIR_RO`/`DIR_RW` file). Return 0 without running.
- `a` → **abort this run**: write `$RUN/.aborted` and `exit 1` the scenario subshell;
  the runner reports it **ABORTED** (distinct from pass/fail), proceeds to the next
  scenario, and implies `KEEP_SCRATCH` so the author can inspect.

Manual mode requires a TTY (refuse otherwise). The runner's `begin` sandbox
scaffolding uses internal `__rm`/`__mkdir` so harness setup isn't prompted.

## DIR_RO / DIR_RW plumbing

- Inherited env passed through untouched. Helpers:
  - `require_dir_ro <label>` / `require_dir_rw <label>` — assert set + is a dir, else
    mark the scenario `skipped` (existing `.skipped`).
  - `it_error <msg>` — assert the previous tool invocation failed (exit != 0) and
    `stderr` contains `msg`.
- Scenario contract: **never write into `DIR_RO`**; `DIR_RW` is fair game. Without the
  fixture a scenario is `skipped`, so automatic runs stay green when neither is set.

## Edit map

- `integration/run.sh` — mode detection (`--manual`/`IT_MODE`), the wrapper set,
  `write_file`, `bin`, gated `ar`/`ex`, `it_prompt`/`it_abort`, `require_dir_ro/rw`,
  `it_error`, aborted classification+counter, `begin` bypass, TTY guard.
- `integration/scenarios/*.sh` (all 19) — `printf…>file → write_file`,
  `"$BIN"… → bin`. No other logic changes.
- New scenarios: `cli_include_from_missing.sh`, `cli_exclude_from_missing.sh`,
  `dir_ro_roundtrip.sh`, `dir_rw_in_place.sh`.
- New `integration/README.md` — manual usage + fixture contract.
- `docs/plans/080-…` — this plan.

## Order

1. Run.sh harness → 2. scenario conversion → 3. new scenarios + README → 4. verify.

## Verification

- Automatic full run green after conversion (fixture scenarios `.skipped`; existing
  scenarios pass; cargo wrapper + `llvm-cov` unaffected — `integration_shell.rs`
  invokes `run.sh <name>` with no `IT_MODE`).
- Manual smoke: exercise `r`/`a`/`s`; confirm `a` → `.aborted`, `s` leaves the target
  untouched and continues, `r` runs, markers+prompt visible despite `>/dev/null`.
- `DIR_RO`/`DIR_RW` scenarios gate on the vars.