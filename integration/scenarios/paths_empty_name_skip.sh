# A transform reducing a member to "" skips it (GNU empty-name sentinel).
scenario_paths_empty_name_skip() {
    begin
    local src="$RUN/src"
    mkdir -p "$src/sub"
    printf 'wanted payload\n' > "$src/keep.txt"
    printf 'dropped payload\n' > "$src/sub/drop.txt"

    ar -i "$src"
    ex --transform 's,^.*/drop\.txt$,,'

    assert_file_eq "$(target "$src/keep.txt")" "$src/keep.txt"
    assert_absent "$(target "$src/sub/drop.txt")"
}