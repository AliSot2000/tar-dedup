- name: Prepare shell
  run: set -euo pipefail
- name: Update apt
  run: sudo apt-get update
- name: Install tar-dedup build dependencies
  run: sudo apt-get install -y build-essential pkg-config libselinux1-dev libacl1-dev libclang-dev clang liblzma-dev libzstd-dev zlib1g-dev libbz2-dev