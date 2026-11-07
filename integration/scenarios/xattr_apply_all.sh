# --apply-all-metadata restores xattr + mode + mtime in one flag.
scenario_xattr_apply_all() {
    begin
    local src="$RUN/src"
    mkdir -p "$src"
    write_file "$src/a.txt" 'x\n'
    chmod 0604 "$src/a.txt"
    local epoch; epoch="$(date -d '2001-02-03 04:05:06' +%s)"
    touch -d "@$epoch" "$src/a.txt"
    setfattr -n user.note -v allmet "$src/a.txt"

    ar -i "$src"
    ex --apply-all-metadata

    local tgt; tgt="$(target "$src/a.txt")"
    assert_xattr "$tgt" user.note allmet
    [ "$(stat -c %a "$tgt")" = 604 ] || fail "mode not restored: $(stat -c %a "$tgt")"
    [ "$(stat -c %Y "$tgt")" = "$epoch" ] || fail "mtime not restored"
}
