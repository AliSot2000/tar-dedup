#!/usr/bin/env python3
"""Count Rust source lines under `crates/` (or any given path) by content class.

For every `*.rs` file the script reports three headline numbers:

- `lines`    — raw physical line count (blank lines included),
- `nonempty` — lines with at least one non-whitespace character,
- `code`     — nonempty lines that are neither a pure comment nor a pure
               docstring, i.e. lines that carry real Rust tokens.

Pure comments are lines whose whole non-whitespace content is a `//` line
comment or sits inside a `/* ... */` block comment. Pure docstrings are the
Rust doc-comment forms `///`, `//!`, `/** ... */`, `/*! ... */`. A line with
any code counts as `code` even when it also carries a trailing comment
(`let x = 1; // note` is code).

Classification is a small character scanner rather than a full Rust lexer; it
still skips string literals (`"..."`, byte `b"..."`, and raw `r"..."` /
`r#"..."#`) so comment markers inside strings do not miscount. Block-comment
and string state are carried across lines.

Usage:

    scripts/rust_line_counts.py [path ...]
"""

import sys
from pathlib import Path

DEFAULT_ROOT = Path(__file__).resolve().parent.parent / "crates"


def classify(line: str, state: dict) -> str:
    """Classify one physical line; `state` (block-comment/string flags) is
    updated in place for the next line. Returns empty|code|comment|doc."""
    if not line.strip():
        return "empty"
    i, n = 0, len(line)
    in_block = state["in_block"]
    block_doc = state["block_doc"]
    in_string = state["in_string"]
    raw_hashes = state["raw_hashes"]
    has_code = has_comment = has_doc = False
    while i < n:
        c = line[i]
        if in_block:
            has_comment = True
            if block_doc:
                has_doc = True
            if c == "*" and i + 1 < n and line[i + 1] == "/":
                in_block = block_doc = False
                i += 2
            else:
                i += 1
            continue
        if in_string:
            if c == '"' and raw_hashes == 0:
                in_string = False
                i += 1
                continue
            if c == '"':
                j = i + 1
                e = 0
                while e < raw_hashes and j < n and line[j] == "#":
                    e += 1
                    j += 1
                if e == raw_hashes:
                    in_string = False
                    raw_hashes = 0
                    i = j
                else:
                    i += 1
                continue
            if c == "\\" and raw_hashes == 0 and i + 1 < n:
                i += 2
                continue
            i += 1
            continue
        if c in "rb" and i + 1 < n:
            j = i + 1
            if c == "b" and line[j] == "r":
                j += 1
            k = j
            while k < n and line[k] == "#":
                k += 1
            if k < n and line[k] == '"':
                has_code = True
                in_string = True
                raw_hashes = k - j
                i = k + 1
                continue
        if c == '"':
            has_code = True
            in_string = True
            raw_hashes = 0
            i += 1
            continue
        if c == "/" and i + 1 < n:
            nxt = line[i + 1]
            if nxt == "/":
                if i + 2 < n and line[i + 2] in "/!":
                    has_doc = True
                else:
                    has_comment = True
                break
            if nxt == "*":
                doc = i + 2 < n and line[i + 2] in "*!"
                close = line.find("*/", i + 2)
                if close != -1:
                    if doc:
                        has_doc = True
                    else:
                        has_comment = True
                    i = close + 2
                else:
                    if doc:
                        has_doc = True
                    else:
                        has_comment = True
                    in_block = True
                    block_doc = doc
                    i = n
                continue
        if c.isspace():
            i += 1
            continue
        has_code = True
        i += 1
    state.update(
        in_block=in_block, block_doc=block_doc,
        in_string=in_string, raw_hashes=raw_hashes,
    )
    if has_code:
        return "code"
    if has_doc:
        return "doc"
    if has_comment:
        return "comment"
    return "empty"


def count_file(path: Path) -> tuple:
    """Return (lines, nonempty, code, comments, docs, empty) for `path`."""
    state = {"in_block": False, "block_doc": False, "in_string": False, "raw_hashes": 0}
    cats = {"empty": 0, "code": 0, "comment": 0, "doc": 0}
    with path.open(encoding="utf-8", errors="replace") as fh:
        for line in fh:
            cats[classify(line.rstrip("\r\n"), state)] += 1
    total = sum(cats.values())
    return (total, total - cats["empty"], cats["code"],
            cats["comment"], cats["doc"], cats["empty"])


def fmt_row(row: list, widths: list) -> str:
    cells = [f"{row[0]:<{widths[0]}}"] + [f"{v:>{w}}" for v, w in zip(row[1:], widths[1:])]
    return "  ".join(cells)


def main() -> int:
    roots = [Path(a) for a in sys.argv[1:]] or [DEFAULT_ROOT]
    files = sorted(p for root in roots for p in root.rglob("*.rs"))
    if not files:
        print("no .rs files found under the given path(s)", file=sys.stderr)
        return 1

    header = ["file", "lines", "nonempty", "code", "comments", "docs", "empty"]
    rows = [header]
    for p in files:
        t, ne, co, cm, dc, em = count_file(p)
        rows.append([str(p), t, ne, co, cm, dc, em])

    totals = [sum(row[i] for row in rows[1:]) for i in range(1, len(header))]
    rows.append(["total", *totals])

    widths = [max(len(str(row[i])) for row in rows) for i in range(len(header))]
    for row in rows:
        print(fmt_row(row, widths))

    print()
    print(f"files: {len(files)}  "
          f"lines: {totals[0]}  nonempty: {totals[1]}  code: {totals[2]}  "
          f"comments: {totals[3]}  docs: {totals[4]}  empty: {totals[5]}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
