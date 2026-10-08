# Extract output root vs `-C`, and compression flags on extract

Agreed design:

1. `-C/--directory` stops being the extraction root on extract — it only sets
   the base for resolving relative CLI paths. A new `--extract-to DIR` selects
   the restore root. **Default `--extract-to` = the `-C`/`--directory` value**
   (i.e. `cwd` when `-C` absent), matching GNU tar: if the working directory is
   moved, extraction follows.
2. Extract gains the compression-selection flags (same `CompressionFlags` set as
   archive) under a Compression help section. Default = infer from the archive
   suffix; any explicit flag overrides the filename; more than one flag → error
   (already enforced by `resolve_compression` + the clap arg group). No
   `--level` / `--xz-extreme` / `--memlimit-compress` on extract — decompression
   only needs the type.

## Changes

### cli.rs (`ExtractArgs`)
- Reword `-C/--directory` help: resolution base only (drop "as the extraction
  root").
- Add under "Archive Paths":
  ```rust
  /// Extract members into DIR (the restore root); default: `-C` / current dir.
  #[arg(long = "extract-to", value_name = "DIR", help_heading = "Archive Paths")]
  pub extract_to: Option<PathBuf>,
  ```
- Add `#[command(flatten, next_help_heading = "Compression")]
  pub compression: CompressionFlags,` (reuses the archive set).

### config/paths.rs (`PathLayout`)
- Add `output_dir: PathBuf` (extraction root); `Default` + clean-path
  `Validate` updated; `extraction_root()` returns `&self.output_dir`.
- Update the `directory` field doc (resolution base now).

### config/extract.rs
- `try_from`:
  ```rust
  let c_directory = resolve_cwd(args.directory.as_deref())?;
  let c_output_dir = match &args.extract_to {
      Some(p) => resolve_path_to_abs_path(p, &c_directory),
      None => c_directory.clone(),
  };
  std::fs::create_dir_all(&c_output_dir)?;
  let decompression = resolve_compression(&args.compression, &c_archive_path)?;
  ...
  None => default_extract_work_dir(&c_archive_path, &c_output_dir, extract_stage_location),
  ...
  paths: PathLayout { archive_path: c_archive_path, directory: c_directory,
                      output_dir: c_output_dir, work_dir },
  ```
- Swap `infer_compression_from_suffix` import for `resolve_compression`.
- Test builders `for_place_test` / `for_scan_test`: add `output_dir`
  (copy of `directory`), keeping extraction-root behavior.

### config/archive.rs + archive test worlds
- `ArchiveConfig::try_from` PathLayout gains `output_dir: directory.clone()`
  (unused on archive).
- 5 archive test-world PathLayout literals (hash/dedup/sparsify/stage/
  tar_builder) gain `output_dir` (empty or `directory`).

## Verification
- `cargo test -p tar-dedup --lib`, `cargo build -p tar-dedup-cli`.
- Manual smoke: `extract -J` on a gzip archive (explicit override),
  `extract --no-auto-compress` on `.tar`; `-C res` + `--extract-to other`
  divergence.

## Notes / decisions
- Behavior change confirmed by user: `-C` no longer defaults the restore root
  implicitly beyond being the default `--extract-to` value (which it still is).
- `ExtractStageLocation` untouched (still hardcoded `BesideArchive`).