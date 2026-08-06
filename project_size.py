#!/usr/bin/env python3
"""Count non-empty lines under crates/ (rough LOC for Rust sources)."""

from __future__ import annotations

import argparse
from pathlib import Path


def count_non_empty_lines(path: Path) -> int:
    total = 0
    with path.open("r", encoding="utf-8", errors="replace") as f:
        for line in f:
            if line.strip():
                total += 1
    return total


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Count non-empty lines of files under crates/."
    )
    parser.add_argument(
        "root",
        nargs="?",
        default="crates",
        type=Path,
        help="Directory to walk (default: crates)",
    )
    args = parser.parse_args()
    root: Path = args.root

    if not root.is_dir():
        raise SystemExit(f"not a directory: {root}")

    grand_total = 0
    file_count = 0
    for path in sorted(root.rglob("*")):
        if not path.is_file():
            continue
        n = count_non_empty_lines(path)
        grand_total += n
        file_count += 1
        print(f"{n:8d}  {path}")

    print(f"{grand_total:8d}  total ({file_count} files)")


if __name__ == "__main__":
    main()
