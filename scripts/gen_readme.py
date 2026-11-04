#!/usr/bin/env python3
"""Regenerate / verify the option tables of `docs/manual.md` from clap metadata.

The single source of truth is the compiled CLI:
`cargo run -p tar-dedup-lib --example gen_help_tables` emits JSON rows grouped
by subcommand + `help_heading`. This script splices those rows into the manual
(`docs/manual.md`):

- description cells are taken verbatim from clap (multi-line docs joined),
- Notes cells of existing rows are carried over by long-name (never rewritten),
- obsolete/removed flags live in `scripts/deprecated.json` and are rendered
  struck through in every column so old descriptions stay copyable; an entry is
  dropped from that file automatically once its struck row is gone from the
  manual (i.e. it was hand-deleted),
- section intros and the numbered Notes block are untouched.

Use `--check` to fail on any drift between the compiled CLI and the manual
(non-struck rows only; struck rows are documentation, not drift).
"""

import argparse
import json
import os
import subprocess
import sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
README = os.path.join(REPO, "docs", "manual.md")
DEPRECATED_JSON = os.path.join(REPO, "scripts", "deprecated.json")


def load_deprecated() -> list:
    """Read `scripts/deprecated.json` (the struck-rows registry)."""
    with open(DEPRECATED_JSON, encoding="utf-8") as fh:
        return json.load(fh)


# Populated by main(); consumed by build_table (render) and check (verify).
DEPRECATED: list = []

# Readme section header overrides to match the clap `help_heading` value.
HEADING_MAP = {"Sidecar Manifests": "Sidecar"}


def clap_rows() -> dict:
    cmd = [
        "cargo", "run", "-q", "-p", "tar-dedup-lib", "--example", "gen_help_tables",
    ]
    out = subprocess.run(cmd, cwd=REPO, capture_output=True, text=True, check=True)
    return json.loads(out.stdout)


def fmt_cell(s: str) -> str:
    return (s or "-").replace("|", "\\|")


HEADER = ["Short", "Long", "Required", "Args", "Description", "Notes"]


def row_cells(r: dict, struck: bool) -> list:
    """One option row as six cell strings (not yet column-padded)."""
    req = "y" if r["required"] is True else ("n" if r["required"] is False else r["required"])
    cells = [fmt_cell(r["short"]), fmt_cell(r["long"]), req, r["args"], fmt_cell(r["description"])]
    notes = r.get("notes", "") or ""
    if struck:
        cells = [f"~~{c}~~" for c in cells]
        if notes:
            notes = f"~~{notes}~~"
    cells.append(notes)
    return cells


def aligned_line(cells: list, widths: list, dashes: bool = False) -> str:
    """One padded table row (`dashes` = the `|-----|` separator row)."""
    if dashes:
        parts = ["-" * w for w in widths]
    else:
        parts = [f" {c:<{w - 2}} " for c, w in zip(cells, widths)]
    return "|" + "|".join(parts) + "|"


def render_table(normal: list, struck: list = None) -> list:
    """Render a padded table; `struck` rows (already cell-lists) become a second,
    struck-through sub-table shared aligned with the header."""
    struck = struck or []
    widths = [2] * 6
    for cells in [HEADER, *normal, *struck]:
        for i, c in enumerate(cells):
            widths[i] = max(widths[i], len(c) + 2)
    lines = [aligned_line(HEADER, widths), aligned_line(None, widths, dashes=True)]
    lines += [aligned_line(c, widths) for c in normal]
    if struck:
        lines.append("<!-- obsolete/deprecated (kept for reference) -->")
        lines.append(aligned_line(HEADER, widths))
        lines.append(aligned_line(None, widths, dashes=True))
        lines += [aligned_line(c, widths) for c in struck]
    return lines


def long_key(cells) -> str:
    return (cells.get("long") or cells.get("short") or "").strip("`~")


def parse_table(block: list) -> dict:
    """old table block -> {long_key: (short, long, req, args, desc, notes)}"""
    out = {}
    for line in block[2:]:
        if not line.startswith("|"):
            continue
        cells = [c.strip() for c in line.strip("|").split("|")]
        if len(cells) != 6:
            continue
        short, long, req, args, desc, notes = cells
        key = (long.strip().strip("`") if long.strip() not in ("-", "") else short.strip().strip("`"))
        out[key] = (short, long, req, args, desc, notes)
    return out


def build_table(json_rows, old_notes, sub, heading, with_obsolete) -> list:
    rows = []
    for r in json_rows:
        key = (r["long"] or r["short"] or "")
        notes = old_notes.get(r["long"] or r["short"], ("",) * 6)[5]
        rows.append({**r, "notes": notes})
    normal = [row_cells(r, struck=False) for r in rows]
    struck = []
    if with_obsolete:
        obs = [d for d in DEPRECATED if d["subcommand"] == sub and d["help_heading"] == heading]
        struck = [
            row_cells(
                {"short": d["short"], "long": d["long"], "required": d["required"],
                 "args": d["args"], "description": d["description"],
                 "notes": d.get("note", "")},
                struck=True,
            )
            for d in obs
        ]
    return render_table(normal, struck)


def is_heading_line(line: str) -> bool:
    s = line.rstrip()
    return (s.startswith("**") and s.endswith("**")) or s.startswith("#### ")


def heading_name(line: str) -> str:
    s = line.rstrip()
    if s.startswith("**"):
        return s.strip("*").strip()
    return s.lstrip("#").strip()


def splice(readme: str, data: dict, drop_obsolete: bool) -> str:
    out = []
    section = None
    i = 0
    lines = readme.splitlines(keepends=True)
    while i < len(lines):
        line = lines[i]
        if line.startswith("### "):
            section = line[4:].strip()
        if is_heading_line(line) and section:
            heading = heading_name(line)
            canonical = HEADING_MAP.get(heading, heading)
            if section in data and any(g["heading"] == canonical for g in data[section]):
                # Keep the heading line (canonical name) + any intro prose, then
                # drop the entire old table region (previous table, obsolete
                # struck block, blank lines) up to the next heading / section.
                # Preserve the original heading's trailing whitespace (e.g. the
                # two-space hard break `**Heading**  \n` before intro prose).
                head = f"**{canonical}**" if line.rstrip().startswith("**") else f"#### {canonical}"
                out.append(f"{head}{line[len(line.rstrip()):]}")
                j = i + 1
                old_block = []
                collecting = False
                # Stop only at a real section heading (`**X**` / `## ` / `### `).
                # A single-`#` comment line (e.g. `#TODO`) is prose, not a
                # boundary, so it must not truncate the region.
                while j < len(lines) and not is_heading_line(lines[j]) \
                        and not lines[j].lstrip().startswith("## ") \
                        and not lines[j].lstrip().startswith("### "):
                    l = lines[j].lstrip()
                    if l.startswith("<!--"):
                        collecting = False  # obsolete struck block follows
                    elif l.startswith("|"):
                        # Collect the first (live) table for the Notes carry-over;
                        # struck rows after the `<!--` marker are dropped.
                        if collecting:
                            old_block.append(lines[j])
                        else:
                            collecting = True
                            old_block.append(lines[j])
                    elif lines[j].strip() == "":
                        # Preserve blank lines inside the intro-prose region
                        # (before the first table row); blanks after the table are
                        # dropped and re-emitted by render_table's own spacing.
                        if not collecting:
                            out.append(lines[j])
                    else:
                        collecting = False
                        out.append(lines[j])  # intro prose
                    j += 1
                # `out` may end with prose but no trailing blank; ensure the
                # table follows a blank line.
                if out and out[-1].strip() != "" and not out[-1].endswith("\n\n"):
                    out.append("\n")
                old_notes = parse_table("".join(old_block).splitlines())
                # Prune the deprecated registry: drop any entry for this table
                # whose struck row is no longer present in the manual (it was
                # hand-deleted) so it is not re-added and stops failing --check.
                present = {opt_key(long, short) for (short, long, *_) in old_notes.values()}
                kept = [d for d in DEPRECATED
                        if not (d["subcommand"] == section and d["help_heading"] == canonical
                                and opt_key(d["long"], d["short"]) not in present)]
                if len(kept) != len(DEPRECATED):
                    DEPRECATED[:] = kept
                rows = next(g["rows"] for g in data[section] if g["heading"] == canonical)
                new_block = build_table(rows, old_notes, section, canonical, not drop_obsolete)
                out.extend(l + "\n" for l in new_block)
                out.append("\n")
                i = j
                continue
        out.append(line)
        i += 1
    return "".join(out)


def opt_key(long: str, short: str) -> str:
    """Normalize a table cell to the canonical option key (backticks/~~ stripped)."""
    v = long if long != "-" else short
    return v.replace("~", "").strip("`")


def check(readme: str, data: dict) -> int:
    """Fail if any non-struck row drifts from clap, or a clap row is missing."""
    errors = []
    lines = readme.splitlines()
    section = None
    i = 0
    while i < len(lines):
        if lines[i].startswith("### "):
            section = lines[i][4:].strip()
            i += 1
            continue
        is_heading = is_heading_line(lines[i])
        if is_heading and section is not None:
            heading = heading_name(lines[i])
            canonical = HEADING_MAP.get(heading, heading)
            if section in data and any(g["heading"] == canonical for g in data[section]):
                rows = next(g["rows"] for g in data[section] if g["heading"] == canonical)
                expected = {r["long"] or r["short"]: r for r in rows}
                obs_keys = {d["long"] for d in DEPRECATED if d["subcommand"] == section and d["help_heading"] == canonical}
                seen = set()
                j = i + 1
                # Scan every row line up to the next bold heading (includes struck
                # obsolete sub-tables).
                while j < len(lines):
                    l = lines[j].lstrip()
                    if is_heading_line(lines[j]):
                        break
                    if lines[j].lstrip().startswith("## ") or lines[j].lstrip().startswith("### "):
                        break
                    if not l.startswith("|"):
                        j += 1
                        continue
                    cells = [c.strip() for c in lines[j].strip("|").split("|")]
                    if len(cells) < 6:
                        j += 1
                        continue
                    short, long, req, args, desc = cells[:5]
                    # Skip the header and the `|-----|` separator rows.
                    if long.strip("`~") == "Long" or (short + long).strip("-") == "":
                        j += 1
                        continue
                    struck = any("~~" in c for c in cells)
                    key = opt_key(long, short)
                    seen.add(key)
                    if not struck:
                        if key in expected:
                            ex = expected[key]
                            if desc != ex["description"]:
                                errors.append(f"{section}/{canonical}: --{key}: description drifted\n"
                                              f"  readme: {desc}\n  clap  : {ex['description']}")
                            if short != (ex["short"] or "-"):
                                errors.append(f"{section}/{canonical}: --{key}: short {short!r} != {ex['short']!r}")
                            want = "y" if ex["required"] else "n"
                            if req != want:
                                errors.append(f"{section}/{canonical}: --{key}: required {req!r} != {want}")
                            if args != ex["args"]:
                                errors.append(f"{section}/{canonical}: --{key}: args {args!r} != {ex['args']!r}")
                        elif key not in obs_keys:
                            errors.append(f"{section}/{canonical}: row --{key} exists in readme but not in clap")
                    j += 1
                for key in expected:
                    if key not in seen:
                        errors.append(f"{section}/{canonical}: --{key} missing from readme")
                for key in obs_keys:
                    if key not in seen:
                        errors.append(f"{section}/{canonical}: obsolete --{key} not struck through")
        i += 1
    if errors:
        print("\n".join(errors))
        return 1
    return 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--check", action="store_true", help="verify the readme, do not write")
    ap.add_argument("--drop-obsolete", action="store_true", help="omit struck obsolete rows")
    args = ap.parse_args()

    global DEPRECATED
    DEPRECATED = load_deprecated()

    data = clap_rows()
    with open(README, encoding="utf-8") as f:
        readme = f.read()

    if args.check:
        return check(readme, data)

    new = splice(readme, data, args.drop_obsolete)
    with open(README, "w", encoding="utf-8") as f:
        f.write(new)
    print("docs/manual.md option tables regenerated from clap metadata")

    if DEPRECATED != load_deprecated():
        with open(DEPRECATED_JSON, "w", encoding="utf-8") as f:
            json.dump(DEPRECATED, f, indent=2, ensure_ascii=False)
            f.write("\n")
        print("scripts/deprecated.json pruned (removed flags no longer struck through)")
    return 0


if __name__ == "__main__":
    sys.exit(main())