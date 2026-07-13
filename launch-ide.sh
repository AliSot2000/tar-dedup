#!/usr/bin/env bash
set -e

case "$1" in
  rust-rover)
    exec nix develop --command rust-rover .
    ;;
  cursor)
    exec nix develop --command cursor .
    ;;
  *)
    echo "Usage: $0 [rust-rover|cursor]"
    exit 1
    ;;
esac
