# xattr on the symlink itself survives l*xattr capture + restore.
# Some filesystems (btrfs) reject `user.*` on symlinks outright (EPERM);
# probe first and skip there.
scenario_xattr_on_symlink() {
    begin
    local src="$RUN/src"
    mkdir -p "$src"
    write_file "$src/a.txt" 'body\n'
    ln -s a.txt "$src/link"

    if ! setfattr -h -n user.probe -v p "$src/link" 2>/dev/null; then
        touch "$RUN/.skipped"
        return 0
    fi
    setfattr -h -x user.probe "$src/link" 2>/dev/null || true
    setfattr -h -n user.link -v linkval "$src/link"

    ar -i "$src"
    ex --xattrs

    local tgt; tgt="$(target "$src/link")"
    [ -L "$tgt" ] || fail "link not recreated at $tgt"
    local got
    got="$(getfattr -h -n user.link --only-values "$tgt" 2>/dev/null || true)"
    [ "$got" = linkval ] || fail "link xattr = '$got', want 'linkval'"
}
