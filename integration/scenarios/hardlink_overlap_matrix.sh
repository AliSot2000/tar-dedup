# Hardlink grouping across overlapping sources x recursion. Pair p1 spans
# b/ ↔ b/c, pair p2 spans b/c ↔ b/c/d.
#   M1 -i b -i b/c            (recursion): both pairs grouped.
#   M2 -i b/c -i b            (order swap): p1 splits (order effect), p2 grouped.
#   M3 -i b/c/d -i b --no-recursion: deep members split/absent.
scenario_hardlink_overlap_matrix() {
    begin
    local b="$RUN/b"
    mkdir -p "$b/c/d"
    printf 'shared payload\n' > "$b/h.txt"
    ln "$b/h.txt" "$b/c/h.txt"
    printf 'deep payload\n' > "$b/c/e.txt"
    ln "$b/c/e.txt" "$b/c/d/e.txt"

    tgt() { printf '%s' "$1/$(printf '%s' "$2" | sed 's|^/||')"; }

    # M1 — the covering source groups everything.
    mkdir -p "$RUN/ext1"
    "$BIN" archive -f "$RUN/m1.astar" -i "$b" -i "$b/c" --no-strict-separation \
        >/dev/null 2>&1 || fail "M1 archive failed"
    "$BIN" extract -f "$RUN/m1.astar" -C "$RUN/ext1" --hard-link-grouping source >/dev/null 2>&1 || fail "M1 extract failed"
    [ "$(stat -c %i "$(tgt "$RUN/ext1" "$b/h.txt")")" = \
       "$(stat -c %i "$(tgt "$RUN/ext1" "$b/c/h.txt")")" ] \
        || fail "M1: b/h and c/h not grouped"
    [ "$(stat -c %i "$(tgt "$RUN/ext1" "$b/c/e.txt")")" = \
       "$(stat -c %i "$(tgt "$RUN/ext1" "$b/c/d/e.txt")")" ] \
        || fail "M1: c/e and c/d/e not grouped"

    # M2 — b/c processed first claims its subtree's side; b's b/h stays separate.
    mkdir -p "$RUN/ext2"
    "$BIN" archive -f "$RUN/m2.astar" -i "$b/c" -i "$b" --no-strict-separation \
        >/dev/null 2>&1 || fail "M2 archive failed"
    "$BIN" extract -f "$RUN/m2.astar" -C "$RUN/ext2" --hard-link-grouping source >/dev/null 2>&1 || fail "M2 extract failed"
    [ "$(stat -c %i "$(tgt "$RUN/ext2" "$b/h.txt")")" != \
       "$(stat -c %i "$(tgt "$RUN/ext2" "$b/c/h.txt")")" ] \
        || fail "M2: b/h and c/h unexpectedly grouped (order effect)"
    [ "$(stat -c %i "$(tgt "$RUN/ext2" "$b/c/e.txt")")" = \
       "$(stat -c %i "$(tgt "$RUN/ext2" "$b/c/d/e.txt")")" ] \
        || fail "M2: c/e and c/d/e should stay grouped"

    # M3 — no-recursion: b sees depth-1 only, b/c/d only d's contents.
    mkdir -p "$RUN/ext3"
    "$BIN" archive -f "$RUN/m3.astar" -i "$b/c/d" -i "$b" \
        --no-strict-separation --no-recursion >/dev/null 2>&1 || fail "M3 archive failed"
    "$BIN" extract -f "$RUN/m3.astar" -C "$RUN/ext3" --hard-link-grouping source >/dev/null 2>&1 || fail "M3 extract failed"
    [ -f "$(tgt "$RUN/ext3" "$b/h.txt")" ] || fail "M3: b/h missing"
    [ -f "$(tgt "$RUN/ext3" "$b/c/d/e.txt")" ] || fail "M3: c/d/e should be present"
    [ ! -e "$(tgt "$RUN/ext3" "$b/c/h.txt")" ] || fail "M3: c/h should be absent (no-recursion)"
    [ ! -e "$(tgt "$RUN/ext3" "$b/c/e.txt")" ] || fail "M3: c/e should be absent (no-recursion)"
}