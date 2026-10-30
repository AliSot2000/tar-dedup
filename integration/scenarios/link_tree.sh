# --link-tree: outputs are symlinks into the materialized payload store at
# <ed>/.sources/ (the canonical copies), reading through to real content.
scenario_link_tree() {
    begin
    local src="$RUN/src"
    mkdir -p "$src"
    printf 'link payload\n' > "$src/payload.txt"

    ar -i "$src"
    ex --link-tree

    local tgt; tgt="$(target "$src/payload.txt")"
    [ -L "$tgt" ] || fail "link-tree output is not a symlink: $tgt"
    local resolved
    resolved="$(readlink -f "$tgt")"
    case "$resolved" in
        "$RUN/ext/.sources/"*) : ;;
        *) fail "link target not under <.sources>: $resolved" ;;
    esac
    assert_file_eq "$tgt" "$src/payload.txt"
}