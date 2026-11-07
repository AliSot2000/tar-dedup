#!/usr/bin/env bash
# End-to-end CLI integration suite for tar-dedup.
#
# Runs a built binary (`target/release/tar-dedup` by default) against real
# filesystem fixtures. Zero cargo needed at runtime: only bash + coreutils +
# `attr` (getfattr / setfattr).
#
# The same scenarios are also exposed as cargo tests
# (`crates/tar-dedup/tests/integration_shell.rs`) so `cargo test` /
# `cargo llvm-cov` drive them and capture the CLI child-process coverage —
# this file is the canonical scenario definition for both entry points.
#
# Usage:
#   integration/run.sh [--manual] [scenario...]   # all scenarios by name order when no args
#   integration/run.sh --list                     # list available scenarios
#   integration/run.sh --help                     # this help
#
# Env:
#   TAR_DEDUP_BIN   override the binary path (default: target/release under repo)
#   SCRATCH         base scratch dir (default: under ~/.cache; the <1 GiB tmpfs
#                   at /tmp is too small for churny integration runs)
#   KEEP_SCRATCH    keep the scratch dir on exit (default: remove)
#   IT_MODE=manual  interactive (also `--manual`) — approve each action via r/a/s
#   DIR_RO / DIR_RW  real-tree fixtures for the dir_ro_* / dir_rw_* scenarios
set -eu

# ------------------------------------------------------------------ bootstrap
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN="${TAR_DEDUP_BIN:-$REPO_DIR/target/release/tar-dedup}"

usage() {
    sed -n '1,/^set -eu/p' "$0" \
        | sed 's|^#||; s|^[[:space:]]*||' \
        | sed '/^$/d; /^set -eu$/d; /^!/d'
}

case "${1:-}" in
    -h|--help)
        usage
        exit 0
        ;;
    -l|--list)
        for f in "$SCRIPT_DIR"/scenarios/*.sh; do
            [ -e "$f" ] || continue
            name="$(basename "$f" .sh)"
            desc="$(sed -n '1s/^[# ]*//p' "$f")" # first # comment line as description
            if [ -n "$desc" ]; then
                printf '%-28s %s\n' "$name" "$desc"
            else
                printf '%s\n' "$name"
            fi
        done
        exit 0
        ;;
esac

if [ ! -x "$BIN" ]; then
    echo "error: binary not found at $BIN (run: cargo build --release -p tar-dedup)" >&2
    exit 2
fi

# default scratch: the <1 GiB tmpfs at /tmp is too small for churny runs.
default_scratch() {
    local d="${XDG_CACHE_HOME:-$HOME/.cache}"
    mkdir -p "$d"
    mktemp -d "$d/tardedup-it.XXXXXX"
}

SCRATCH="${SCRATCH:-$(default_scratch)}"
export SCRATCH

if [ "${KEEP_SCRATCH:-0}" != "1" ]; then
    # NOTE: __rm (not `rm`) so the harness's own cleanup never trips the gating
    # wrapper / an interactive prompt.
    trap '__rm -rf "$SCRATCH"' EXIT
fi

# -------------------------------------------------------------------- manual mode
# IT_MODE=manual (env) or a leading `--manual` arg selects interactive mode: every
# mutating setup command and every tar-dedup invocation is shown on the terminal and
# approved with r(un)/a(bort)/s(elf).
IT_MODE="${IT_MODE:-}"
ARGS=("$@")
if [ "${ARGS[0]:-}" = "--manual" ]; then
    IT_MODE=manual
    ARGS=("${ARGS[@]:1}")
fi
if [ "$IT_MODE" = manual ]; then
    if ! ( : <>/dev/tty ) 2>/dev/null; then
        echo "error: manual mode requires a controlling terminal (/dev/tty)" >&2
        exit 3
    fi
    echo "== interactive mode: each action is shown; choose r(un)/a(bort)/s(elf) ==" >&2
fi
export IT_MODE

PASS=0; FAILURES=0; ABORTED=0
FAILED_NAMES=()

# -------------------------------------------------------------------- helpers
fail() {
    echo "    FAIL: $*" >&2
    FAILURES=$((FAILURES + 1))
    return 1
}

# begin: fresh run dir with a pre-existing extraction root. Uses the internal
# `__rm`/`__mkdir` so the harness's own sandbox scaffolding is never prompted.
begin() {
    RUN="$SCRATCH/run-$SCENARIO_NAME"
    __rm -rf "$RUN"
    __mkdir -p "$RUN/ext"
}

# ar / ex / bin: archive / extract / arbitrary tar-dedup invocation. Gated in
# manual mode (each tool call is approved); automatic mode calls through as before.
ar() {
    if [ "$IT_MODE" = manual ]; then
        it_prompt tool "tar-dedup archive -f $RUN/out.astar $*"
        case "$?" in
            3) return 0;;
            2) it_abort;;
        esac
    fi
    "$BIN" archive -f "$RUN/out.astar" "$@"
}
ex() {
    if [ "$IT_MODE" = manual ]; then
        it_prompt tool "tar-dedup extract -f $RUN/out.astar -C $RUN/ext $*"
        case "$?" in
            3) return 0;;
            2) it_abort;;
        esac
    fi
    "$BIN" extract -f "$RUN/out.astar" -C "$RUN/ext" "$@"
}
bin() {
    if [ "$IT_MODE" = manual ]; then
        it_prompt tool "tar-dedup $*"
        case "$?" in
            3) return 0;;
            2) it_abort;;
        esac
    fi
    "$BIN" "$@"
}

# target: the path extraction maps `$abs` to under the scenario ext root.
target() { printf '%s' "$RUN/ext/$(printf '%s' "$1" | sed 's|^/||')"; }

assert_file_eq() { # path expected-content
    local expect
    expect="$(cat "$2")"
    [ "$(cat "$1")" = "$expect" ] \
        || fail "content mismatch at $1 (got '$(cat "$1")', want '$expect')"
}

assert_absent() {
    [ ! -e "$1" ] || fail "expected absent: $1"
}

assert_xattr() { # path name expected
    local got
    got="$(getfattr -n "$2" --only-values "$1" 2>/dev/null || true)"
    [ "$got" = "$3" ] || fail "xattr $2 at $1 = '$got', want '$3'"
}

assert_no_xattr() { # path name
    if getfattr -n "$2" --only-values "$1" >/dev/null 2>&1; then
        fail "xattr $2 unexpectedly present at $1"
    fi
}

assert_xattr_hex() { # path name expected-raw-bytes(base64)
    # NB: getfattr ignores `-e base64` in --only-values mode; pipe the raw value.
    local got
    got="$(getfattr -n "$2" --only-values "$1" 2>/dev/null | base64)"
    [ "$got" = "$3" ] || fail "xattr $2(base64) at $1 = '$got', want '$3'"
}

# ------------------------------------------------------- interactive (manual-mode)
# Internal (never-prompted) coreutils shims used by the harness itself.
__mkdir() { command mkdir "$@"; }
__rm()    { command rm "$@"; }

# Abort the current scenario run: mark `$RUN/.aborted`, exit non-zero (run_one
# classifies it as ABORTED, not a failure) and leaves the scratch dir for inspection.
it_abort() {
    __mkdir -p "$RUN" 2>/dev/null || true
    : > "$RUN/.aborted"
    echo "   ABORTED by user" >&2
    exit 1
}

# Prompt handler → 0 = run, 3 = self(skip), 2 = abort. Marker + prompt go to
# /dev/tty so per-command `>/dev/null` redirects can't hide them.
it_prompt() { # kind cmdline
    local kind="$1" cmdline="$2" choice=""
    printf '[%s]  $ %s\n' "$kind" "$cmdline" >/dev/tty
    while :; do
        printf '[r]un / [a]bort / [s]elf > ' >/dev/tty
        if ! IFS= read -r choice </dev/tty; then
            it_abort
        fi
        case "$choice" in
            r|R) return 0;;
            s|S) return 3;;
            a|A) return 2;;
            *)   printf '  (valid: r/a/s)\n' >/dev/tty ;;
        esac
    done
}

# Generic wrapper: gate (in manual) then run the real command via `command`.
_prog_guard() { # name args...
    local name="$1"; shift
    if [ "$IT_MODE" = manual ]; then
        it_prompt setup "$name $*"
        case "$?" in
            3) return 0;;
            2) it_abort;;
        esac
    fi
    command "$name" "$@"
}

# write_file <path> <fmt> [args...]: file content via an enforced printf. Uses an
# internal redirection so choosing `s` can never clobber an author-made file.
write_file() { # path fmt [args...]
    local p="$1"; shift
    if [ "$IT_MODE" = manual ]; then
        it_prompt setup "write_file $p : printf '$1'"
        case "$?" in
            3) return 0;;
            2) it_abort;;
        esac
    fi
    { printf "$@"; } > "$p"
}

# Shadow mutating utilities: on `s` return 0 without running (set -e safe).
for __c in mkdir cp ln rm rmdir mv setfattr setfacl chmod chown touch truncate install mknod mkfifo; do
    eval "function $__c() { _prog_guard '$__c' \"\$@\"; }"
done
unset __c

# require_dir_ro / require_dir_rw: gate a scenario on the matching fixture var;
# when absent the scenario is marked skipped (exits 0, run_one reports skipped).
require_dir() { # varname
    local v="$1"
    if [ -z "${!v:-}" ] || [ ! -d "${!v}" ]; then
        __mkdir -p "$RUN"
        : > "$RUN/.skipped"
        echo "   skipped: $v not set or not a directory" >&2
        exit 0
    fi
}
require_dir_ro() { require_dir DIR_RO; }
require_dir_rw() { require_dir DIR_RW; }

# it_error <msg>: after an `if bin ...; then fail ...; fi` (which already proves the
# tool exited non-zero), assert the CLI error message landed on $RUN/err.txt.
it_error() { # msg
    grep -q "$1" "$RUN/err.txt" || { fail "CLI error did not mention '$1'"; return 1; }
}

# ---------------------------------------------------------------- scenarios loader
SCENARIO_DIR="$SCRIPT_DIR/scenarios"
SCENARIOS=()
for f in "$SCENARIO_DIR"/*.sh; do
    [ -e "$f" ] || continue
    # shellcheck disable=SC1090
    source "$f"
    SCENARIOS+=("$(basename "$f" .sh)")
done

# ---------------------------------------------------------------------- runner
run_one() {
    SCENARIO_NAME="$1"
    local fn="scenario_$1"
    # Scenarios run in a subshell and can't export RUN back out; compute it here
    # too so the skip / abort markers are found.
    local RUN="$SCRATCH/run-$SCENARIO_NAME"
    if ! declare -F "$fn" >/dev/null 2>&1; then
        echo "error: no scenario named '$1'" >&2
        return 2
    fi
    echo "== scenario: $1"
    if ( set -e; "$fn" ); then
        if [ -e "$RUN/.skipped" ]; then
            echo "   skipped"
        else
            PASS=$((PASS + 1))
            echo "   ok"
        fi
    elif [ -e "$RUN/.aborted" ]; then
        ABORTED=$((ABORTED + 1))
        KEEP_SCRATCH=1
        export KEEP_SCRATCH
        echo "   ABORTED"
    else
        FAILURES=$((FAILURES + 1))
        FAILED_NAMES+=("$1")
        echo "   FAILED"
    fi
}

TARGETS=("${ARGS[@]}")
if [ "${#TARGETS[@]}" -eq 0 ]; then
    TARGETS=("${SCENARIOS[@]}")
fi

for name in "${TARGETS[@]}"; do
    run_one "$name" || [ "$?" -eq 0 ]
done

echo
echo "integration: $PASS passed, $ABORTED aborted, ${#FAILED_NAMES[@]} failed"
if [ "${#FAILED_NAMES[@]}" -gt 0 ]; then
    printf '  failed: %s\n' "${FAILED_NAMES[@]}"
    exit 1
fi
exit 0
