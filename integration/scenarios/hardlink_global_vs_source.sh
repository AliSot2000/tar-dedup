# Hardlink grouping across sibling sources: global re-links the cross-source
# hardlinked pair; source keeps each source's group separate.
scenario_hardlink_global_vs_source() {
    begin
    local s1="$RUN/s1" s2="$RUN/s2"
    mkdir -p "$s1" "$s2"
    printf 'cross payload\n' > "$s1/cross.txt"
    ln "$s1/cross.txt" "$s2/cross2.txt"
    printf 'own payload\n' > "$s1/own.txt"

    ar -i "$s1" -i "$s2"

    tgt() { printf '%s' "$1/$(printf '%s' "$2" | sed 's|^/||')"; }

    mkdir -p "$RUN/extg"
    "$BIN" extract -f "$RUN/out.astar" -C "$RUN/extg" --hard-link-grouping global \
        >/dev/null 2>&1 || fail "global extract failed"
    [ "$(stat -c %i "$(tgt "$RUN/extg" "$s1/cross.txt")")" = \
       "$(stat -c %i "$(tgt "$RUN/extg" "$s2/cross2.txt")")" ] \
        || fail "global did not group the cross-source pair"

    mkdir -p "$RUN/exts"
    "$BIN" extract -f "$RUN/out.astar" -C "$RUN/exts" --hard-link-grouping source \
        >/dev/null 2>&1 || fail "source extract failed"
    [ "$(stat -c %i "$(tgt "$RUN/exts" "$s1/cross.txt")")" != \
       "$(stat -c %i "$(tgt "$RUN/exts" "$s2/cross2.txt")")" ] \
        || fail "source grouped across sources"
}