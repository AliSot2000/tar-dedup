#!/usr/bin/env bash
# Build a self-contained "toolchain dir" that RustRover can be pointed at as a
# MANUAL toolchain.
#
# The nix (fenix) rustc/cargo bundle the llvm-tools component in the sysroot,
# which is all coverage needs: both cargo-llvm-cov and grcov locate the LLVM
# tools themselves via `rustc --print sysroot` (grcov: llvm_tools.rs -> rustlib
# -> $sysroot/lib/rustlib/<host>/bin). No rustup is involved.
#
# IMPORTANT: the wrapper intentionally contains NO `rustup`. If a `rustup`
# binary sits in the toolchain dir, RustRover treats the toolchain as
# rustup-managed and resolves the active toolchain (and hence the analyzer
# sysroot) through rustup -> the wrong ~/.rustup install -> "std is missing" /
# FnMut/Option/Vec unresolved. With a plain manual toolchain, rust-analyzer uses
# `rustc --print sysroot` directly and the bundled rust-src (present here) loads.
#
# Usage: rustrover-toolchain.sh [output-dir]
#   output-dir defaults to ~/.rustrover-toolchain
#
# Point RustRover at $OUT as a MANUAL location
# (Settings -> Languages & Frameworks -> Rust -> toolchain) and launch RustRover
# from inside `nix develop` so the wrapper (cargo/rustc/grcov/llvm tools) also
# lands on PATH.

set -euo pipefail

OUT="${1:-$HOME/.rustrover-toolchain}"

# The real (nix) rustup for shim delegation — resolve it excluding our own dir.
REAL_RUSTUP="${REAL_RUSTUP:-$(PATH="$(printf '%s' "$PATH" | tr ':' '\n' | grep -vx "$OUT/bin" | paste -sd:)" command -v rustup 2>/dev/null || true)}"

# Resolve an executable from PATH, never from our own wrapper dir (which can be
# first on PATH and would otherwise self-link).
resolve_exe() {
  PATH="$(printf '%s' "$PATH" | tr ':' '\n' | grep -vx "$OUT/bin" | paste -sd:)" \
    command -v "$1" 2>/dev/null || true
}

RUSTC="$(resolve_exe rustc)"
SYS="$("$RUSTC" --print sysroot)"
RUST_BIN_DIR="$(dirname -- "$RUSTC")"
TRIPLE="$("$RUSTC" -vV | sed -n 's/^host: //p')"
LLVM_BIN="$SYS/lib/rustlib/$TRIPLE/bin"

mkdir -p "$OUT/bin"

# 1. The fenix toolchain binaries (rustc, cargo, rustfmt, clippy, rust-analyzer, ...).
for f in "$RUST_BIN_DIR"/*; do
  [ -f "$f" ] && ln -sfn "$f" "$OUT/bin/$(basename "$f")"
done

# 2. The sysroot llvm-tools (llvm-cov, llvm-profdata, llvm-objdump, opt, ...).
#    These do not collide with the toolchain binary names.
for f in "$LLVM_BIN"/*; do
  [ -f "$f" ] && ln -sfn "$f" "$OUT/bin/$(basename "$f")"
done

# 3. Make the dir look like a complete toolchain root: `lib`/`etc` mirror the
#    sysroot (rust-src std library lives at $OUT/lib/rustlib/src/rust/library,
#    same layout as a rustup toolchain dir). The IDE only resolves the stdlib
#    via `rustc --print sysroot` anyway, but a complete layout keeps every
#    probe happy and survives rustup-style path expectations.
ln -sfn "$SYS/lib" "$OUT/lib"
ln -sfn "$SYS/etc" "$OUT/etc"

# 4. grcov (cargo-installed into ~/.cargo/bin, or on PATH) so RustRover's
#    coverage backend finds it next to the toolchain.
GRCOV="$(resolve_exe grcov)"
if [ -z "$GRCOV" ] && [ -x "$HOME/.cargo/bin/grcov" ]; then
  GRCOV="$HOME/.cargo/bin/grcov"
fi
if [ -n "$GRCOV" ]; then
  ln -sfn "$GRCOV" "$OUT/bin/grcov"
fi

# 4. A `rustup` shim in the toolchain dir. RustRover coverage calls
#    `checkLlvmTools` which needs BOTH a `rustup` executable next to the
#    toolchain (else "Cannot use Llvm-tools: rustup not found") AND its
#    `component list` to report llvm-tools-preview installed (else it offers to
#    install into the wrong rustup). The shim answers those against the nix
#    sysroot and delegates everything else to the real rustup. This does NOT
#    affect the linter's std resolution (that is fixed by the stdlib-local-copy
#    seed below).
cat > "$OUT/bin/rustup" <<EOF
#!/usr/bin/env bash
SYS="$SYS"
TRIPLE="$TRIPLE"
LLVM="$SYS/lib/rustlib/$TRIPLE/bin"
REAL="$REAL_RUSTUP"
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
  toolchain)
    if [ "\${2:-}" = "list" ]; then
      echo "nix-stable-\$TRIPLE (active, default)"
      exit 0
    fi
    ;;
  which)
    name="\${@: -1}"
    case "\$name" in
      cargo|rustc|rustdoc|rustfmt|clippy-driver|cargo-fmt|cargo-clippy|rust-analyzer)
        echo "\$SYS/bin/\$name"; exit 0 ;;
      llvm-tools) echo "\$LLVM"; exit 0 ;;
      *)
        if [ -x "\$LLVM/\$name" ]; then
          echo "\$LLVM/\$name"; exit 0
        fi
        ;;
    esac
    ;;
  run)
    if [ -n "\${3:-}" ] && [ -x "\$SYS/bin/\$3" ]; then
      shift 2
      exec "\$SYS/bin/\$@"
    fi
    ;;
  default)
    echo "nix-stable-\$TRIPLE (default)"
    exit 0
    ;;
esac
exec "\$REAL" "\$@"
EOF
chmod +x "$OUT/bin/rustup"

# 5. cargo-llvm-cov (the source-based coverage runner wants it next to the
#    toolchain / in ~/.cargo/bin; the nix shell provides it on PATH).
if CLLVM="$(resolve_exe cargo-llvm-cov)"; then
  ln -sfn "$CLLVM" "$OUT/bin/cargo-llvm-cov"
fi

# 6. Seed RustRover's stdlib-local-copy cache (fixes "std is missing").
#    RustRover copies the sysroot rust-src into
#      ~/.cache/JetBrains/RustRover*/intellij-rust/stdlib-local-copy/<semver>-<hash>/
#    and loads the std workspace from there — but only copies when the dir is
#    MISSING (`Files.exists` check); an empty dir left by a failed first copy is
#    then used forever. The copy also chokes on the nix store's read-only
#    symlink farm. So we PRE-CREATE + populate the exact dir (its name is a
#    deterministic SHA-1 mix of the src path and the rustc commit hash,
#    replicated from the plugin's HashCode.kt) with a dereferenced, writable
#    copy of the sources. The plugin then finds it populated and skips.
SEED_VER="$("$RUSTC" --version | awk '{print $2}')"
SEED_COMMIT="$("$RUSTC" --version --verbose | awk '/^commit-hash:/{print $2}')"
SEED_SRC="$SYS/lib/rustlib/src/rust"
seed_dir() {
  local d="$1"
  if [ ! -d "$d" ] || [ -z "$(ls -A "$d")" ]; then
    mkdir -p "$d"
    # cp -rL copies the source's read-only dir modes; chmod AFTER so the seed
    # is fully writable (read-only dirs would also break the IDE cache cleanup).
    cp -rL "$SEED_SRC/." "$d/"
    chmod -R u+w "$d"
    echo "  seeded stdlib-local-copy: $d"
  fi
}
# hex on stdin -> raw bytes on stdout (xxd when present, python3 fallback).
hex2bin() {
  if command -v xxd >/dev/null 2>&1; then
    xxd -r -p
  else
    python3 -c 'import sys; sys.stdout.buffer.write(bytes.fromhex(sys.stdin.read().strip()))'
  fi
}
# The cache path derives from the config dirs (config survives cache wipes; the
# per-version suffix matches).
for cfg in "$HOME"/.config/JetBrains/RustRover*/; do
  [ -d "$cfg" ] || continue
  base="$HOME/.cache/JetBrains/$(basename "$cfg")/intellij-rust/stdlib-local-copy"
  mkdir -p "$base"
  if [ -n "$SEED_COMMIT" ]; then
    h1=$(printf '%s' "$SEED_SRC" | sha1sum | awk '{print $1}')
    h2=$(printf '%s' "$SEED_COMMIT" | sha1sum | awk '{print $1}')
    h=$( ( printf '%s' "$h1" | hex2bin; printf '%s' "$h2" | hex2bin ) | sha1sum | awk '{print $1}')
    seed_dir "$base/$SEED_VER-$h"
  fi
  # fallback: any other empty <ver>-* dir from an earlier failed attempt.
  for d in "$base"/"$SEED_VER"-*/; do
    [ -d "$d" ] || continue
    [ -z "$(ls -A "$d")" ] && seed_dir "$d"
  done
done

echo "RustRover toolchain ready at $OUT (manual; no rustup)"
echo "  rustc   = $SYS/bin/rustc"
echo "  llvm    = $LLVM_BIN"
[ -n "${GRCOV:-}" ] && echo "  grcov   = $GRCOV"