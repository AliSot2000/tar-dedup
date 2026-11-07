# Relative `--input-dir ../..` must be clamped under the extraction root.
scenario_paths_relative_input() {
    begin
    local cwd="$RUN/cwd" src="$RUN/src"
    mkdir -p "$cwd" "$src"
    write_file "$src/r.txt" 'relative payload\n'

    bin archive -f "$RUN/out.astar" -C "$cwd" -i ../src >/dev/null 2>&1
    bin extract -f "$RUN/out.astar" -C "$RUN/ext" >/dev/null 2>&1

    # `../src` is clamped to `src` under the extraction root.
    assert_file_eq "$RUN/ext/src/r.txt" "$src/r.txt"

    local rogue
    rogue="$(find "$RUN" -path "$RUN/ext" -prune -o \
        -path "$RUN/src" -prune -o -path "$RUN/cwd" -prune -o \
        -type f -print | grep -v '/out.astar$' || true)"
    [ -z "$rogue" ] || fail "file escaped extraction root: $rogue"
}