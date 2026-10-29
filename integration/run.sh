#!/usr/bin/env bash
# End-to-end CLI integration suite for tar-dedup.
#
# Runs a built binary (`target/release/tar-dedup` by default) against real
# filesystem fixtures. Zero cargo needed at runtime: only bash + coreutils +
# `attr` (getfattr / setfattr).
#
# The same scenarios are also exposed as cargo tests
# (`crates/tar-dedup-cli/tests/integration_shell.rs`) so `cargo test` /
# `cargo llvm-cov` drive them and capture the CLI child-process coverage —
# this file is the canonical scenario definition for both entry points.
#
# Usage:
#   integration/run.sh [scenario...]     # all by name order when no args
#
# Env:
#   TAR_DEDUP_BIN   override the binary path (default: target/release under repo)
#   SCRATCH         base scratch dir (default: under ~/.cache; the <1 GiB tmpfs
#                   at /tmp is too small for churny integration runs)
#   KEEP_SCRATCH    keep the scratch dir on exit (default: remove)
set -eu

# ------------------------------------------------------------------ bootstrap
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN="${TAR_DEDUP_BIN:-$REPO_DIR/target/release/tar-dedup}"

if [ ! -x "$BIN" ]; then
    echo "error: binary not found at $BIN (run: cargo build --release -p tar-dedup-cli)" >&2
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
    trap 'rm -rf "$SCRATCH"' EXIT
fi

PASS=0; FAILURES=0
FAILED_NAMES=()

# -------------------------------------------------------------------- helpers
fail() {
    echo "    FAIL: $*" >&2
    FAILURES=$((FAILURES + 1))
    return 1
}

# begin: fresh run dir with a pre-existing extraction root.
begin() {
    RUN="$SCRATCH/run-$SCENARIO_NAME"
    rm -rf "$RUN"
    mkdir -p "$RUN/ext"
}

# ar / ex: archive / extract against the scenario's fixed names.
ar() { "$BIN" archive -f "$RUN/out.astar" "$@"; }
ex() { "$BIN" extract -f "$RUN/out.astar" -C "$RUN/ext" "$@"; }

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
    # too so the skip marker is found.
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
    else
        FAILURES=$((FAILURES + 1))
        FAILED_NAMES+=("$1")
        echo "   FAILED"
    fi
}

TARGETS=("$@")
if [ "${#TARGETS[@]}" -eq 0 ]; then
    TARGETS=("${SCENARIOS[@]}")
fi

for name in "${TARGETS[@]}"; do
    run_one "$name" || [ "$?" -eq 0 ]
done

echo
echo "integration: $PASS passed, ${#FAILED_NAMES[@]} failed"
if [ "${#FAILED_NAMES[@]}" -gt 0 ]; then
    printf '  failed: %s\n' "${FAILED_NAMES[@]}"
    exit 1
fi
exit 0
