# Exercise a genuine, read-only user-supplied tree (DIR_RO) end to end: archive it,
# extract it, and byte-verify a representative regular file against the source by
# reading DIR_RO. Requires DIR_RO; skipped when unset or empty.
scenario_dir_ro_roundtrip() {
    require_dir_ro
    begin
    local sample
    sample="$(find "$DIR_RO" -type f -print -quit)"
    if [ -z "$sample" ]; then
        __mkdir -p "$RUN"; : > "$RUN/.skipped"
        echo "   skipped: DIR_RO has no regular files" >&2
        exit 0
    fi

    ar -i "$DIR_RO"
    ex

    local tgt="$RUN/ext/${sample#/}"
    [ -f "$tgt" ] || { fail "DIR_RO member not extracted: $sample"; return 1; }
    cmp -s "$sample" "$tgt" || fail "DIR_RO member content differs after round-trip: $sample"
}