#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"

case "$1" in
  rust-rover)
    # Prepare the REAL rustup toolchain for the IDE (writable std sources ->
    # stable linter; rustup shim + ~/.cargo/bin tools -> coverage). The user
    # points RustRover's toolchain location at the printed <toolchain>/bin.
    bash "$ROOT/scripts/rustrover-rustup.sh" >/dev/null 2>&1 || \
      echo "warning: could not prepare rustup toolchain for RustRover" >&2
    exec nix develop --command rust-rover . &
    ;;
  cursor)
    exec nix develop --command cursor . &
    ;;
  *)
    echo "Usage: $0 [rust-rover|cursor]"
    exit 1
    ;;
esac