# Non-UTF-8 xattr value round-trips byte-identically (base64 comparison).
scenario_xattr_binary() {
    begin
    local src="$RUN/src"
    mkdir -p "$src"
    write_file "$src/bin.dat" 'blob\n'
    local expect_b64
    expect_b64="$(printf '\x01\x02\xff\xfe\x5a' | base64)"
    setfattr -n user.blob -v "$(printf '\x01\x02\xff\xfe\x5a')" "$src/bin.dat"

    ar -i "$src"
    ex --xattrs

    assert_xattr_hex "$(target "$src/bin.dat")" user.blob "$expect_b64"
}
