# GNU order: `--transform` runs before `--strip-components`.
scenario_paths_strip_transform_order() {
    begin
    local src="$RUN/src"
    mkdir -p "$src/a/l1/l2"
    write_file "$src/a/l1/l2/f.txt" 'order payload\n'

    ar -i "$src"

    # Member = abs path with leading '/' stripped; anchor on its first component.
    local rel first
    rel="$(printf '%s' "$src/a/l1/l2/f.txt" | sed 's|^/||')"
    first="$(printf '%s' "$rel" | cut -d/ -f1)"

    # Transform rewrites the first component to x/x/, then one component is
    # stripped: transform-first lands at ed/x/<rest>.
    ex --transform "s,^${first}/,x/x/," --strip-components 1

    local expected="$RUN/ext/x/${rel#${first}/}"
    [ -f "$expected" ] || fail "transform-then-strip target missing: $expected"
    assert_file_eq "$expected" "$src/a/l1/l2/f.txt"

    # Strip-first would drop a component BEFORE the transform: `^$first/` no
    # longer matches, so the x/ prefix would be missing.
    local wrong="$RUN/ext/${rel#${first}/}"
    [ ! -e "$wrong" ] || fail "strip-first ordering produced: $wrong"
}