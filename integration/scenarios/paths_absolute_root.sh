# Absolute input dir: leading '/' drops, everything lands under <ed>.
scenario_paths_absolute_root() {
    begin
    local src="$RUN/src"
    mkdir -p "$src"
    printf 'abs payload\n' > "$src/a.txt"

    ar -i "$src"
    ex

    assert_file_eq "$(target "$src/a.txt")" "$src/a.txt"
}