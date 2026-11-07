# A transform reducing a member to "" skips it (GNU empty-name sentinel).
scenario_paths_empty_name_skip() {
    begin
    local src="$RUN/src"
    mkdir -p "$src/sub"
    write_file "$src/keep.txt" 'wanted payload\n'
    write_file "$src/sub/drop.txt" 'dropped payload\n'

    ar -i "$src"
    ex --transform 's,^.*/drop\.txt$,,'

    assert_file_eq "$(target "$src/keep.txt")" "$src/keep.txt"
    assert_absent "$(target "$src/sub/drop.txt")"
}