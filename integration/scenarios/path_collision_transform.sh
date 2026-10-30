# Two members collapsed onto one target by --transform: first wins on disk, the
# other member is displaced (error recorded at unit level, plans/058).
scenario_path_collision_transform() {
    begin
    local src="$RUN/src"
    mkdir -p "$src"
    printf 'first payload\n' > "$src/f1.txt"
    printf 'second payload\n' > "$src/f2.txt"

    ar -i "$src"

    local transform='s,^.*/f[12]\.txt$,merged.txt,'
    ex --transform "$transform"

    local tgt="$RUN/ext/merged.txt"
    [ -f "$tgt" ] || fail "collided target missing: $tgt"
    local got one second
    one="$(cat "$src/f1.txt")"
    second="$(cat "$src/f2.txt")"
    got="$(cat "$tgt")"
    { [ "$got" = "$one" ] || [ "$got" = "$second" ]; } || fail "collided target content: '$got'"

    # Neither original member extracts under its own name.
    assert_absent "$(target "$src/f1.txt")"
    assert_absent "$(target "$src/f2.txt")"

    # --no-errors parity: same disk outcome, still exit 0.
    mkdir -p "$RUN/ext2"
    "$BIN" extract -f "$RUN/out.astar" -C "$RUN/ext2" --no-errors --transform "$transform" \
        >/dev/null 2>&1 || fail "extract with --no-errors failed"
    local got2
    got2="$(cat "$RUN/ext2/merged.txt" 2>/dev/null || true)"
    [ "$got2" = "$got" ] || fail "no-errors run changed the winner: '$got2' vs '$got'"
}