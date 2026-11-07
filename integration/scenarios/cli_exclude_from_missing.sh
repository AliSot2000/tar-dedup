# --exclude-from must resolve to an existing file at CLI validation; a missing one
# errors out (exit non-zero) before any archiving happens.
scenario_cli_exclude_from_missing() {
    begin
    local src="$RUN/src"
    mkdir -p "$src"
    write_file "$src/a.txt" 'payload\n'

    if ar -i "$src" --exclude-from "$RUN/nope-exclude" >"$RUN/err.txt" 2>&1; then
        fail "archive unexpectedly succeeded with a missing --exclude-from"
        return 1
    fi
    it_error "exclude-from"
}