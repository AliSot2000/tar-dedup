# Not-yet-implemented subcommands (list/dump/query/reset) resolve their catalog
# source and then fail with a clean "not implemented yet" error — this keeps
# `cmd::resolve_catalog_source` and the main dispatch arms exercised.
scenario_cmd_stub_not_implemented() {
    begin
    local src="$RUN/src"
    mkdir -p "$src"
    write_file "$src/a.txt" 'payload\n'

    ar -i "$src"

    for sub in list dump query; do
        if bin "$sub" -f "$RUN/out.astar" >/dev/null 2>"$RUN/err.txt"; then
            fail "$sub unexpectedly succeeded"
        fi
        grep -q "not implemented" "$RUN/err.txt" \
            || fail "$sub did not report not-implemented"
    done

    # Leave an interrupted archive so reset has a work db to resolve.
    bin archive -f "$RUN/reset.astar" -i "$src" --exit-after-stage inventory \
        >/dev/null 2>&1
    if bin reset --work-dir "$RUN/reset.astage" >/dev/null 2>"$RUN/err.txt"; then
        fail "reset unexpectedly succeeded"
    fi
    grep -q "not implemented" "$RUN/err.txt" \
        || fail "reset did not report not-implemented"
}