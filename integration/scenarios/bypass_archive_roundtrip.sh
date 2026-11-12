# Bypass archive round-trip: archive -f <dir> --bypass-archive produces a product
# dir (canonical payloads + manifest.sqlite); extract -f <dir> --bypass-archive
# validates it in place, materializes the output tree from a disposable work DB,
# and leaves the immutable manifest untouched.
scenario_bypass_archive_roundtrip() {
    begin
    local src="$RUN/src"
    mkdir -p "$src/sub"
    write_file "$src/hello.txt" 'hello bypass\n'
    cp "$src/hello.txt" "$src/sub/dup.txt"

    # Archive into a *directory* product, not a tar (uses bin; the `ar` helper
    # hardcodes `-f $RUN/out.tdd.tar`).
    bin archive --bypass-archive -f "$RUN/product" -i "$src"
    [ -f "$RUN/product/manifest.sqlite" ] || fail "baked manifest missing"
    [ ! -f "$RUN/product/tar-dedup.sqlite" ] || fail "archive left a work db"

    # Extract from the product dir; the dir doubles as the work dir.
    bin extract --bypass-archive -f "$RUN/product" -C "$RUN/ext"

    assert_file_eq "$(target "$src/hello.txt")" "$src/hello.txt"
    assert_file_eq "$(target "$src/sub/dup.txt")" "$src/sub/dup.txt"
    # The product dir (the "archive") and its manifest survive extraction; the
    # disposable work db is dropped on a clean run.
    [ -f "$RUN/product/manifest.sqlite" ] || fail "manifest must survive extract"
    [ ! -f "$RUN/product/tar-dedup.sqlite" ] || fail "work db should be cleaned after Done"
}