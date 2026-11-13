#!/usr/bin/env bash
# Sync the working tree to the test VM and run the full test suite there.
#
# The VM is Fedora/btrfs/SELinux (needed for the full xattr/ACL/SELinux suites),
# and its /tmp is a small tmpfs, so tests run with TMPDIR on the btrfs-backed
# home. The tree is synced cleanly (target/.git/mnt/graphify-out excluded); the
# remote checkout is wiped first so deleted local files never linger as stale
# test binaries.
#
# Env:
#   VM_HOST         ssh destination      (default: test-user@192.168.56.101)
#   VM_KEY          ssh identity file    (default: ~/.ssh/tar-dedup-vm)
#   VM_REPO         remote checkout dir  (default: /home/test-user/tar-dedup)
#   VM_TMPDIR       remote TMPDIR        (default: /home/test-user/tmp)
#   TEST_FEATURES   cargo features        (default: test-all)
#   CARGO_TEST_ARGS extra `cargo test` args, e.g. "-- --nocapture"
set -euo pipefail

VM_HOST="${VM_HOST:-test-user@192.168.56.101}"
VM_KEY="${VM_KEY:-$HOME/.ssh/tar-dedup-vm}"
VM_REPO="${VM_REPO:-/home/test-user/tar-dedup}"
VM_TMPDIR="${VM_TMPDIR:-/home/test-user/tmp}"
TEST_FEATURES="${TEST_FEATURES:-test-all}"
CARGO_TEST_ARGS="${CARGO_TEST_ARGS:-}"

LOCAL_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SSH=(ssh -o BatchMode=yes -o StrictHostKeyChecking=no -o IdentityAgent=none -i "$VM_KEY")

echo "==> syncing $LOCAL_ROOT -> $VM_HOST:$VM_REPO"
"${SSH[@]}" "$VM_HOST" "rm -rf '$VM_REPO' && mkdir -p '$VM_REPO'" \
    && tar cf - -C "$LOCAL_ROOT" \
        --exclude=target --exclude=.git --exclude=mnt --exclude=graphify-out . \
    | "${SSH[@]}" "$VM_HOST" "tar xf - -C '$VM_REPO'"

echo "==> cargo test --features $TEST_FEATURES (on $VM_HOST)"
set +e
"${SSH[@]}" "$VM_HOST" \
    "export PATH=\$HOME/.cargo/bin:\$PATH TMPDIR='$VM_TMPDIR'; \
     cd '$VM_REPO' && mkdir -p '$VM_TMPDIR' \
     && cargo test --features '$TEST_FEATURES' $CARGO_TEST_ARGS"
status=$?
set -e

echo "==> cleaning up $VM_TMPDIR"
"${SSH[@]}" "$VM_HOST" \
    "chmod -R u+rwx '$VM_TMPDIR' 2>/dev/null; rm -rf '$VM_TMPDIR' 2>/dev/null" || true

if [[ $status -eq 0 ]]; then
    echo "==> all tests passed on $VM_HOST"
else
    echo "==> tests FAILED on $VM_HOST (exit $status)" >&2
fi
exit $status
