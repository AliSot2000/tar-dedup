# --include-from must resolve to an existing file at CLI validation; a missing one
# errors out (exit non-zero) before any archiving happens.
scenario_cli_include_from_missing() {
    begin
    local src="$RUN/src"
    mkdir -p "$src"
    write_file "$src/a.txt" 'payload\n'

    if ar -i "$src" --include-from "$RUN/nope-include" >"$RUN/err.txt" 2>&1; then
        fail "archive unexpectedly succeeded with a missing --include-from"
        return 1
    fi
    it_error "include-from"
}