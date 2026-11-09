# Integration suite (`integration/`)

End-to-end CLI scenarios for `tar-dedup` (`run.sh` + `scenarios/*.sh`). The same
scenarios are exposed as cargo tests (`crates/tar-dedup/tests/integration_shell.rs`)
so `cargo test` / `llvm-cov` drive them.

## Automatic mode (default)

```sh
integration/run.sh                 # all scenarios, by name order
integration/run.sh dedup_roundtrip # a single scenario
integration/run.sh --list          # list scenarios (+ their first comment line)
integration/run.sh --help          # usage / env reference
```

No prompts; each scenario sets up its own scratch fixture under `$SCRATCH`
(default `~/.cache/tardedup-it.XXXXXX`, removed on exit unless `KEEP_SCRATCH=1`).
A release binary is required: `cargo build --release -p tar-dedup`
(`TAR_DEDUP_BIN` overrides the path).

## Manual / interactive mode

Same scenarios, but every **mutating setup command** (`mkdir`/`cp`/`ln`/`rm`/
`setfattr`/`chmod`/`touch`… and the file-content helper `write_file`) **and** every
`tar-dedup` invocation is echoed to the terminal and approved before it runs:

```text
[setup]  $ mkdir -p /ea/run-dedup_roundtrip/src
[r]un / [a]bort / [s]elf > _
```

- `r` — run the command.
- `s` (**self**) — you already performed it by hand; the script skips it (and trusts
  the result). Useful with `DIR_RO`/`DIR_RW`: prepare a real fixture by hand, then
  answer `s` on the matching commands.
- `a` (**abort**) — stop this scenario run. It is reported as **ABORTED** (not a
  pass/fail) and the scratch dir is left in place (`KEEP_SCRATCH` implied) for
  inspection.

Activate with `--manual` (recommended, single scenario) or `IT_MODE=manual`:

```sh
integration/run.sh --manual dedup_roundtrip
IT_MODE=manual integration/run.sh xattr_user
```

Manual mode requires a controlling terminal (`/dev/tty`). Markers and prompts go to
`/dev/tty` so per-command `>/dev/null` redirects can't hide them.

## Real-file fixtures (`DIR_RO` / `DIR_RW`)

Two environment variables point a scenario at genuine user-made trees:

- **`DIR_RO`** — an existing directory to **read only** as an archive source
  (`ar -i "$DIR_RO"`). The tool only reads here; scenarios must never write into it.
- **`DIR_RW`** — a preexisting, **read-write** directory the tool may write into /
  modify (a realistic non-empty target).

Scenarios that need a fixture declare it with `require_dir_ro` / `require_dir_rw`;
when the var is unset (or not a directory) they report **skipped**, so an automatic
run stays green without them. Example:

```sh
DIR_RO=/home/user/Documents/backup-sources integration/run.sh --manual dir_ro_roundtrip
```

## Authoring scenarios

Scenarios are functions `scenario_<name>()` in `scenarios/<name>.sh`, sourced by
`run.sh`. Available helpers:

- `begin` — fresh sandbox `$RUN` with a pre-existing `$RUN/ext` extraction root.
- `ar` / `ex` — `tar-dedup archive` / `extract` against `$RUN/out.tdd.tar`.
- `bin <args…>` — any other `tar-dedup` invocation.
- `write_file <path> <fmt> [args…]` — create a file (gateable, `printf`-formatted).
- `target <abs-path>` — where extract maps an absolute source path (`$RUN/ext/<abs>`).
- `assert_file_eq` / `assert_absent` / `assert_xattr` / `assert_xattr_hex` /
  `assert_no_xattr` — content / presence / xattr checks.
- `require_dir_ro` / `require_dir_rw` — gate a scenario on a fixture (skip if absent).
- `it_error <msg>` — after an `if ar …; then fail; fi`, assert the CLI error message
  (`"$RUN/err.txt"`) mentions `<msg>`.
- `fail <msg>` — mark the scenario failed.

Mutating commands are shadowed by gating wrappers in manual mode; automatic mode
calls through to the real binaries unchanged. Prefer `write_file` over a bare
`printf … > file` so manual **s** can't truncate a hand-made file.