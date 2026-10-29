# exit-after-stage inventory, then resume --work-dir finishes the archive.
scenario_resume_archive() {
    begin
    local src="$RUN/src"
    mkdir -p "$src"
    local i
    for i in $(seq 1 40); do printf 'row %s\n' "$i" > "$src/f$i.txt"; done

    ar -i "$src" --exit-after-stage inventory > "$RUN/step1.log" 2>&1
    [ -d "$RUN/out.astage" ] || fail "work dir missing after exit-after-stage"
    [ ! -e "$RUN/out.astar" ] || fail "archive written before it should be"

    "$BIN" resume --work-dir "$RUN/out.astage" > "$RUN/step2.log" 2>&1
    [ -f "$RUN/out.astar" ] || fail "archive not completed after resume"

    ex --xattrs
    assert_file_eq "$(target "$src/f1.txt")" "$src/f1.txt"
    assert_file_eq "$(target "$src/f40.txt")" "$src/f40.txt"
}
