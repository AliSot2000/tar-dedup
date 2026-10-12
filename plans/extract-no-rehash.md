# Extract `--no-rehash` flag + move `rehash` out of `ScanOptions`

Agreed design: the rehash phase is a pure on/off toggle ("do it, don't do it"),
so no wrapper struct is warranted. `rehash` becomes a top-level `bool` on
`ExtractConfig`, and the extract CLI gains a `--no-rehash` flag (default: on)
under a dedicated **Rehash** help heading.

## Changes

### cli.rs (`ExtractArgs`)
```rust
/// Do not run the rehash (content verification) phase. Default: on.
#[arg(long = "no-rehash", default_value_t = false, action = ArgAction::SetTrue, help_heading = "Rehash")]
pub no_rehash: bool,
```

### config/extract.rs
- `ScanOptions`: remove `rehash` (field + `Default`).
- `ExtractConfig`: add `pub rehash: bool` (default `true`), near `scan`.
  - serde: field gets `#[serde(default = "default_true")]` + `fn default_true() -> bool { true }`
    so stored work-DB configs still deserialize.
- `try_from`: `rehash: !args.no_rehash`.
- `merge_over`: `rehash: merge_pick_copy(self.rehash, base.rehash, default.rehash)`.
- `for_place_test` / `for_scan_test`: `rehash: true,`.

### Consumers
- `unarchive/rehash.rs:45` → `if !rt.config.rehash`.
- `db/integrity.rs:632,679` → `config.rehash`.
- `unarchive/rehash.rs:935` (test) → `world.config.rehash = false;`.

## Verification
- `cargo build -p tar-dedup-cli` + `cargo test -p tar-dedup --lib`.
- `extract --help` shows `--no-rehash` under `Rehash:`.
- The stale `clear_archive_meta` meta test failure is unrelated/pre-existing.