# Work inside a preexisting, read-write user-supplied tree (DIR_RW): create a
# hardlink pair, archive it, and extract into a sibling dir under DIR_RW, then check
# the tool preserved the hardlink and the content. Requires DIR_RW; skipped when
# unset.
scenario_dir_rw_in_place() {
    require_dir_rw
    begin
    local src="$DIR_RW/.it-src" out="$DIR_RW/.it-out"
    mkdir -p "$src" "$out"
    write_file "$src/a.txt" 'rw payload\n'
    ln "$src/a.txt" "$src/a2.txt"

    ar -i "$src"
    bin extract -f "$RUN/out.tdd.tar" -C "$out" --hard-link-grouping global

    local tgt_a="$out/${src#/}/a.txt" tgt_b="$out/${src#/}/a2.txt"
    [ "$(stat -c %i "$tgt_a")" = "$(stat -c %i "$tgt_b")" ] \
        || fail "DIR_RW tree did not preserve the hardlink"
    assert_file_eq "$tgt_a" "$src/a.txt"
}