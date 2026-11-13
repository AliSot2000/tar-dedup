#!/usr/bin/env bash
# Start the graphify MCP server (stdio) with a background file watcher.
#
#   graphify watch <repo>   rebuilds graphify-out/graph.json on code changes (AST-only)
#   graphify-mcp <graph>    MCP stdio server; hot-reloads graph.json by mtime/size
#                           on the next query, so watch updates are picked up live.
#
# stdout is the MCP JSON-RPC channel and must stay clean, so every watcher log line
# is redirected to graphify-out/watch.log (override with GRAPHIFY_WATCH_LOG). Both
# children are killed when the server exits or the launcher is signalled.
#
# Env:
#   GRAPHIFY_REPO        repo root to watch        (default: this script's parent dir)
#   GRAPHIFY_MCP_BIN     path to graphify-mcp      (default: PATH lookup)
#   GRAPHIFY_BIN         path to graphify          (default: PATH lookup)
#   GRAPHIFY_NO_WATCH=1  serve only, skip the watcher
#   GRAPHIFY_WATCH_LOG   watcher log file          (default: <repo>/graphify-out/watch.log)
set -euo pipefail

REPO_ROOT="${GRAPHIFY_REPO:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
OUT_DIR="$REPO_ROOT/graphify-out"
GRAPH="$OUT_DIR/graph.json"
LOG="${GRAPHIFY_WATCH_LOG:-$OUT_DIR/watch.log}"

GRAPHIFY_BIN="${GRAPHIFY_BIN:-$(command -v graphify || true)}"
MCP_BIN="${GRAPHIFY_MCP_BIN:-$(command -v graphify-mcp || true)}"

if [[ ! -f "$GRAPH" ]]; then
    echo "[graphify-mcp] no graph at $GRAPH — build one first (/graphify or graphify extract)" >&2
    exit 1
fi
if [[ -z "$MCP_BIN" ]]; then
    echo "[graphify-mcp] graphify-mcp not found on PATH (install: pip install 'graphifyy[mcp]')" >&2
    exit 1
fi

mkdir -p "$OUT_DIR"

watcher_pid=""
mcp_pid=""
cleanup() {
    [[ -n "$mcp_pid" ]] && kill "$mcp_pid" 2>/dev/null || true
    [[ -n "$watcher_pid" ]] && kill "$watcher_pid" 2>/dev/null || true
    wait 2>/dev/null || true
}
trap 'cleanup; exit 0' INT TERM HUP
trap cleanup EXIT

if [[ -n "${GRAPHIFY_NO_WATCH:-}" && "${GRAPHIFY_NO_WATCH}" != "0" ]]; then
    echo "[graphify-mcp] watcher disabled (GRAPHIFY_NO_WATCH=${GRAPHIFY_NO_WATCH})" >&2
elif [[ -z "$GRAPHIFY_BIN" ]]; then
    echo "[graphify-mcp] graphify not found on PATH — serving a static graph (no watch)" >&2
else
    "$GRAPHIFY_BIN" watch "$REPO_ROOT" >>"$LOG" 2>&1 &
    watcher_pid=$!
    echo "[graphify-mcp] watching $REPO_ROOT (pid $watcher_pid, log $LOG)" >&2
fi

# Keep the real stdin on fd 3: a backgrounded command in a non-interactive shell
# would otherwise have its stdin redirected from /dev/null, starving the MCP
# server of its JSON-RPC input.
exec 3<&0
"$MCP_BIN" --graph "$GRAPH" <&3 &
mcp_pid=$!
exec 3<&-
wait "$mcp_pid" || true
