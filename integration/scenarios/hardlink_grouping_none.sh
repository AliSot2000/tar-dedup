# --hard-link-grouping none: no extraction-time linking — every file its own
# inode, nlink 1, contents intact (None sanity check).
scenario_hardlink_grouping_none() {
    begin
    local src="$RUN/src"
    mkdir -p "$src"
    write_file "$src/h1.txt" 'group payload\n'
    ln "$src/h1.txt" "$src/h2.txt"
    write_file "$src/solo.txt" 'solo payload\n'

    ar -i "$src"
    ex --hard-link-grouping none

    local i1 i2 is
    i1="$(stat -c %i "$(target "$src/h1.txt")")"
    i2="$(stat -c %i "$(target "$src/h2.txt")")"
    is="$(stat -c %i "$(target "$src/solo.txt")")"
    [ "$i1" != "$i2" ] || fail "none must not group h1/h2 (inode $i1)"
    [ "$i1" != "$is" ] && [ "$i2" != "$is" ] || fail "none must not group solo"
    [ "$(stat -c %h "$(target "$src/h1.txt")")" = 1 ] || fail "nlink > 1 under none"
    assert_file_eq "$(target "$src/h1.txt")" "$src/h1.txt"
    assert_file_eq "$(target "$src/solo.txt")" "$src/solo.txt"
}