# Overlapping -i roots with --no-strict-separation: every file present exactly
# once with correct content. Real hardlinks must stay detectable (no
# --no-hardlink-detection).
scenario_no_strict_separation_fanout() {
    begin
    local top="$RUN/top" sub="$RUN/top/sub"
    mkdir -p "$sub"
    printf 'shared payload a\n' > "$top/a.txt"
    printf 'shared payload s\n' > "$sub/s.txt"
    ln "$top/a.txt" "$sub/a2.txt"
    printf 'own payload\n' > "$top/b.txt"

    ar -i "$top" -i "$sub" --no-strict-separation
    ex

    assert_file_eq "$(target "$top/a.txt")" "$top/a.txt"
    assert_file_eq "$(target "$sub/a2.txt")" "$sub/a2.txt"
    assert_file_eq "$(target "$sub/s.txt")" "$sub/s.txt"
    assert_file_eq "$(target "$top/b.txt")" "$top/b.txt"
}