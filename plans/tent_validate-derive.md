# TENTATIVE — `Validate` derive macro (field-cascade `validate()`)

Status: **tentative / not agreed**. Explores whether a `#[derive(Validate)]`
style macro is worth the infra vs a simple shared helper. Nothing here is
settled; revisit before implementing.

## Goal

For a config block where **every field's type implements `Validate`**, generate
a `validate()` that calls each child's `validate()` in order, short-circuiting on
the first error — without hand-writing the delegation in every container
(`ExtractConfig`, `ArchiveConfig`, future blocks).

Semantics: the generated impl type-checks **iff all field types implement
`Validate`** (a missing impl is a compile error in the generated code), which is
exactly "if all my struct fields implement the validate trait".

## Background / current state

- `crate::config::Validate` (config.rs) — plain trait, `fn validate(&self) -> Result<()>`.
- Manual delegations today:
  - `ArchiveConfig::validate` → `sparse` + `process` + `pipeline`.
  - `ExtractConfig::validate` → `placement` + `scan` + `process`.
- This std supports user-defined derive macros (`#[proc_macro_derive]`,
  `proc_macro::TokenStream`); `clap_derive` (`Parser`, `Args`, `ValueEnum`, …) is
  the in-tree precedent.

## Design — custom derive crate

1. New crate `crates/tar-dedup-derive` (proc-macro crate, dependency of
   `tar-dedup`, **not** of anything else):
   - `use proc_macro::TokenStream; use syn::{DeriveInput, DataStruct, Fields, parse_macro_input};`
   - export:
     `#[proc_macro_derive(Validate)] pub fn validate(input: TokenStream) -> TokenStream`
2. Generation for a struct:
   ```
   impl Validate for {Ty} {
       fn validate(&self) -> crate::error::Result<()> {
           self.{f0}.validate()?;
           self.{f1}.validate()?;
           ...
           Ok(())
       }
   }
   ```
   - Field list via `syn::Fields` on the derive input (same API `clap_derive`
     uses for `Args`).
   - Skip fields whose type is `Option<T>`? No — delegate `Option<T>.validate()`
     would need an `Option` shim. Decide per policy (see open questions).
   - Skip private/internal fields? Derives see all named fields; same question
     as `Clone` (skip fields the struct marks as `#[validate(skip)]`?).
3. Enum input: no fields → generate a no-op `validate()` (or `Ok(())`).
4. Attribute knobs (optional): `#[validate(skip = "field")]`, or a marker for
   fields that must be skipped (e.g. `PathLayout.directory` vs `output_dir`).

## Where it applies

- `config.rs` `Validate` trait stays as-is (derive implements it).
- Switch `ArchiveConfig` / `ExtractConfig` to the derive; drop the manual
  field-cascade (keep any container-specific invariants either above the derive
  or in a wrapper that calls the derived helper).
- Sub-structs (`PlacementOptions`, `ScanOptions`, `ProcessOptions`,
  `ArchivePipelineOptions`, `SparseOptions`) already hand-implement `Validate`
  — they become the "leaves" the derive calls into.

## Alternatives (fallback if not worth it)

`config/common.rs` helper (no macro crate):

```rust
/// Run each child validator in order, short-circuiting on the first error.
pub(crate) fn validate_children(children: &[&impl Fn() -> Result<()>]) -> Result<()> {
    for child in children { child()? }
    Ok(())
}
```

`ExtractConfig::validate` becomes:
```rust
validate_children(&[
    || self.placement.validate(),
    || self.scan.validate(),
    || self.process.validate(),
])?;
```

## Open questions

1. `Option<T>` fields: auto-delegate via an `Option<T>` `Validate` shim in this
   crate (only if `T: Validate`), or require containers to flatten (current
   `PlacementOptions` uses `Option<PathBuf>` → no `Validate`)? Recommended:
   provide `impl Validate for Option<T: Validate>` in the crate so derives
   compose.
2. Field skipping (`#[validate(skip)]`) — needed? `PathLayout` fields like
   `archive_path`/`directory` have no `Validate` but PathLayout is not
   currently a child of a derived container... confirm containment before
   deciding.
3. Derive outputs container-specific extras? The derive generates only the
   field-cascade; anything extra stays hand-written (calling the derived part
   under a second name like `validate_fields` if needed).
4. New crate naming/location; add to workspace + `[dependencies]` of `tar-dedup`.
5. Tests: how to unit-test a derive (compile-side; structure like `clap_derive`
   tests or rely on `cargo build` of the crate + a few derived structs).

## Rough steps

1. `crates/tar-dedup-derive` scaffold (Cargo.toml, proc-macro entry, `derive_validate`).
2. Implement struct/enum generation using `syn` (mirror `clap_derive::derives`).
3. Wire `use tar_dedup_derive::` + `#[derive(Validate)]` on the container configs.
4. Remove manual cascades; keep per-container invariants.
5. Build + `cargo test -p tar-dedup --lib`; verify all derived structs compile.
6. If friction exceeds the one-file helper, abandon the derive and ship
   `validate_children` instead.

## Decision point

Only proceed past step 2 if the derive stays small and reads clearly; otherwise
the helper in `config/common.rs` is the recommended fallback (same semantics,
~10 lines, zero infra).