# Same-content files dedup to one canonical; each member keeps its own xattr.
scenario_dedup_roundtrip() {
    begin
    local src="$RUN/src" dup="$RUN/src/dup"
    mkdir -p "$dup"
    printf 'same content here\n' > "$src/a.txt"
    cp "$src/a.txt" "$dup/b.txt"
    setfattr -n user.mark -v first "$src/a.txt"
    setfattr -n user.mark -v second "$dup/b.txt"

    ar -i "$src"
    ex --xattrs

    assert_file_eq "$(target "$src/a.txt")" "$src/a.txt"
    assert_file_eq "$(target "$dup/b.txt")" "$src/a.txt"
    assert_xattr "$(target "$src/a.txt")" user.mark first
    assert_xattr "$(target "$dup/b.txt")" user.mark second
}
