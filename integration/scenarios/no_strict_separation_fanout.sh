# Overlapping -i roots with --no-strict-separation: every file present exactly
# once with correct content. Real hardlinks must stay detectable (no
# --no-hardlink-detection).
scenario_no_strict_separation_fanout() {
    begin
    local top="$RUN/top" sub="$RUN/top/sub"
    mkdir -p "$sub"
    write_file "$top/a.txt" 'shared payload a\n'
    write_file "$sub/s.txt" 'shared payload s\n'
    ln "$top/a.txt" "$sub/a2.txt"
    write_file "$top/b.txt" 'own payload\n'

    ar -i "$top" -i "$sub" --no-strict-separation
    ex

    assert_file_eq "$(target "$top/a.txt")" "$top/a.txt"
    assert_file_eq "$(target "$sub/a2.txt")" "$sub/a2.txt"
    assert_file_eq "$(target "$sub/s.txt")" "$sub/s.txt"
    assert_file_eq "$(target "$top/b.txt")" "$top/b.txt"
}