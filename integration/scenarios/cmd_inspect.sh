# `inspect` subcommand: text + json on a finished archive.
scenario_cmd_inspect() {
    begin
    local src="$RUN/src"
    mkdir -p "$src"
    write_file "$src/a.txt" 'payload\n'

    ar -i "$src"

    local text
    text="$(bin inspect -f "$RUN/out.tdd.tar" 2>/dev/null)"
    [ -n "$text" ] || fail "inspect text produced no output"

    local json
    json="$(bin inspect -f "$RUN/out.tdd.tar" --json 2>/dev/null)"
    printf '%s' "$json" | grep -q '"archive_size"' \
        || fail "inspect --json lacks archive_size"
    printf '%s' "$json" | grep -q '"tool_version"' \
        || fail "inspect --json lacks tool_version"
}