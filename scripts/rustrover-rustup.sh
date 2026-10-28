#!/usr/bin/env bash
# Prepare the REAL rustup-managed toolchain for RustRover (the wiki approach).
#
# Why this instead of the fenix wrapper: RustRover's `ToolchainSourcesFetcher`
# DELETES its stdlib-local-copy and re-copies from the toolchain's rust-src on
# every sync. The fenix rust-src lives in the read-only nix store, so that
# re-copy fails -> empty dir -> "std is missing" (and it wipes any pre-seed).
# The rustup toolchain (~/.rustup) is writable real files, so the re-copy
# always succeeds and the linter stays stable.
#
# Coverage additionally requires a `rustup` executable NEXT TO the toolchain
# whose `component list` reports llvm-tools installed (else "Cannot use
# Llvm-tools: rustup not found"), plus grcov / cargo-llvm-cov findable via the
# toolchain bin or ~/.cargo/bin. This script installs all of that.
#
# Usage: rustrover-rustup.sh [toolchain-name]
#   toolchain-name defaults to the rustup default (or stable).
#
# Point Settings -> Rust toolchain location at the printed <toolchain>/bin.

set -euo pipefail

RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"

TC_NAME="${1:-}"
if [ -z "$TC_NAME" ]; then
  if [ -f "$RUSTUP_HOME/settings.toml" ]; then
    TC_NAME="$(sed -n 's/^default_toolchain *= *"\([^"]*\)"/\1/p' "$RUSTUP_HOME/settings.toml" | head -1)"
  fi
fi
TC_NAME="${TC_NAME:-stable-x86_64-unknown-linux-gnu}"
TC="$RUSTUP_HOME/toolchains/$TC_NAME"

[ -d "$TC/bin" ] || { echo "error: toolchain $TC_NAME not found at $TC" >&2; exit 1; }
TRIPLE="$("$TC/bin/rustc" -vV | sed -n 's/^host: //p')"
SYS="$("$TC/bin/rustc" --print sysroot)"
[ -d "$SYS/lib/rustlib/src/rust/library" ] || {
  echo "error: toolchain $TC_NAME has no rust-src in $SYS (rustup component add rust-src)" >&2
  exit 1
}

# `rustup` shim next to the toolchain: coverage gate (component list reports
# llvm-tools installed) and llvm-tool resolution; delegates everything else to
# the real rustup (resolved on PATH, falling back to the bare name).
REAL="$(command -v rustup 2>/dev/null || echo rustup)"
cat > "$TC/bin/rustup" <<EOF
#!/usr/bin/env bash
SYS="$SYS"
TRIPLE="$TRIPLE"
LLVM="$SYS/lib/rustlib/$TRIPLE/bin"
REAL="$REAL"
case "\$1" in
  --version) echo "rustup 1.29.0 (nix shim)"; exit 0 ;;
  component)
    if [ "\${2:-}" = "list" ]; then
      printf '%s\n' \
        "rustc-\$TRIPLE (installed)" \
        "cargo-\$TRIPLE (installed)" \
        "rust-std-\$TRIPLE (installed)" \
        "rust-src (installed)" \
        "rustfmt-preview-\$TRIPLE (installed)" \
        "clippy-preview-\$TRIPLE (installed)" \
        "llvm-tools-preview-\$TRIPLE (installed)"
      exit 0
    fi
    ;;
  which)
    name="\${@: -1}"
    case "\$name" in
      cargo|rustc|rustdoc|rustfmt|clippy-driver|rust-analyzer)
        echo "\$SYS/bin/\$name"; exit 0 ;;
      llvm-tools) echo "\$LLVM"; exit 0 ;;
      *)
        if [ -x "\$LLVM/\$name" ]; then
          echo "\$LLVM/\$name"; exit 0
        fi
        ;;
    esac
    ;;
esac
exec "\$REAL" "\$@"
EOF
chmod +x "$TC/bin/rustup"

# Coverage backend executables: the plugin looks in the toolchain bin, then
# ~/.cargo/bin. grcov is nix-provided (not cargo-installed) so it lands in
# ~/.cargo/bin; cargo-llvm-cov is usually already there. Exclude any stale
# fenix-wrapper dir from PATH so we never link through it.
mkdir -p "$HOME/.cargo/bin"
GRCOV="$(PATH="$(printf '%s' "$PATH" | tr ':' '\n' | grep -vE 'rustrover-toolchain/bin' | paste -sd:)" command -v grcov 2>/dev/null || true)"
if [ -n "$GRCOV" ]; then
  ln -sfn "$GRCOV" "$HOME/.cargo/bin/grcov"
fi
[ -x "$HOME/.cargo/bin/cargo-llvm-cov" ] || \
  { CL="$(PATH="$(printf '%s' "$PATH" | tr ':' '\n' | grep -vE 'rustrover-toolchain/bin' | paste -sd:)" command -v cargo-llvm-cov 2>/dev/null || true)"; [ -n "$CL" ] && ln -sfn "$CL" "$HOME/.cargo/bin/cargo-llvm-cov"; }

echo "RustRover rustup toolchain ready: $TC/bin"
echo "  rustc   = $SYS/bin/rustc ($("$TC/bin/rustc" --version))"
echo "  llvm    = $SYS/lib/rustlib/$TRIPLE/bin"
echo "  grcov   = $HOME/.cargo/bin/grcov"
echo "  Point Settings -> Rust toolchain location at: $TC/bin"