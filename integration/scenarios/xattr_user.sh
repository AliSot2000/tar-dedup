# user.* xattr round-trip on regular files (--xattrs opt-in).
scenario_xattr_user() {
    begin
    local src="$RUN/src" sub="$RUN/src/sub"
    mkdir -p "$sub"
    printf 'alpha payload\n' > "$src/a.txt"
    printf 'beta payload\n' > "$sub/b.bin"
    setfattr -n user.author -v alice "$src/a.txt"
    setfattr -n user.note -v 'hello xattr' "$sub/b.bin"

    ar -i "$src"
    ex --xattrs

    assert_xattr "$(target "$src/a.txt")" user.author alice
    assert_xattr "$(target "$sub/b.bin")" user.note 'hello xattr'
    assert_file_eq "$(target "$src/a.txt")" "$src/a.txt"
}
