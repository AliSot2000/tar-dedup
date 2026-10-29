# Extract without --xattrs / --apply-all-metadata applies nothing (by design).
scenario_xattr_none_default() {
    begin
    local src="$RUN/src"
    mkdir -p "$src"
    printf 'x\n' > "$src/a.txt"
    setfattr -n user.note -v present "$src/a.txt"

    ar -i "$src"
    ex

    assert_no_xattr "$(target "$src/a.txt")" user.note
}
