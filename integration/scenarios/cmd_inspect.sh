# `inspect` subcommand: text + json on a finished archive.
scenario_cmd_inspect() {
    begin
    local src="$RUN/src"
    mkdir -p "$src"
    printf 'payload\n' > "$src/a.txt"

    ar -i "$src"

    local text
    text="$("$BIN" inspect -f "$RUN/out.astar" 2>/dev/null)"
    [ -n "$text" ] || fail "inspect text produced no output"

    local json
    json="$("$BIN" inspect -f "$RUN/out.astar" --json 2>/dev/null)"
    printf '%s' "$json" | grep -q '"archive_size"' \
        || fail "inspect --json lacks archive_size"
    printf '%s' "$json" | grep -q '"tool_version"' \
        || fail "inspect --json lacks tool_version"
}