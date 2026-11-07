# A transform injecting ../ must stay clamped under the extraction root.
scenario_path_clamp() {
    begin
    local src="$RUN/src"
    mkdir -p "$src"
    write_file "$src/payload.txt" 'payload\n'

    ar -i "$src"

    # The extract transform runs on the relative member (abs path with the
    # leading '/' stripped); rewrite its first component to inject a `../` that
    # the out_tree clamp must pull back under the extraction root.
    local rel first
    rel="$(printf '%s' "$src" | sed 's|^/||')"      # home/test-user/.../run-*/src
    first="$(printf '%s' "$rel" | cut -d/ -f1)"     # home
    ex --transform "s,^${first},../clamped,"

    local expected="$RUN/ext/clamped/${rel#${first}/}/payload.txt"
    [ -f "$expected" ] || fail "clamped target missing: $expected"

    # Nothing else escaped the extraction root (the source tree and the archive
    # itself are expected; everything else must stay under the ext root).
    local rogue
    rogue="$(find "$RUN" \
        -path "$RUN/ext" -prune -o \
        -path "$RUN/src" -prune -o \
        -type f -print | grep -v '/out.astar$' || true)"
    [ -z "$rogue" ] || fail "file escaped extraction root: $rogue"
}
