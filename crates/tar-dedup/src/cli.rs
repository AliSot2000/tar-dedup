use std::path::PathBuf;

use clap::{ArgAction, Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "tar-dedup", about = "Deduplicating archival pipeline")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Walk, deduplicate, and write a resumable archive.
    Archive(ArchiveArgs),
    /// Restore a tar-dedup archive (materialize: full copy at each original path).
    Extract(ExtractArgs),
}

#[derive(Debug, Args)]
pub struct ArchiveArgs {
    // --- Archive / paths ---

    /// Archive path (relative paths use `-C` / current directory).
    #[arg(short = 'f', value_name = "ARCHIVE")]
    pub archive: PathBuf,

    /// Treat DIR as the working directory when resolving relative paths on this
    /// command line (`-f`, `-i`, `-T`, `--work-dir`, …). Absolute paths are
    /// unchanged. Default: process current directory.
    #[arg(short = 'C', long = "directory", value_name = "DIR")]
    pub directory: Option<PathBuf>,

    /// Stage directory for sqlite DB, staged payloads, and locks (defaults to
    /// `{stem}.astage` next to the archive). Relative paths use `-C` / cwd.
    #[arg(long = "work-dir", value_name = "DIR")]
    pub work_dir: Option<PathBuf>,

    // Input processing

    /// Input directory to snapshot (repeatable; union of roots).
    #[arg(
        short = 'i',
        long = "input-dir",
        value_name = "DIR",
        action = ArgAction::Append
    )]
    pub input_dirs: Vec<PathBuf>,

    /// Read member paths from FILE (`-` = stdin). Repeatable; union with `-i`.
    #[arg(
        short = 'T',
        long = "files-from",
        value_name = "FILE",
        action = ArgAction::Append
    )]
    pub files_from: Vec<PathBuf>,

    /// `-T` records are NUL-terminated (default: newline-separated).
    #[arg(long = "null", default_value_t = false)]
    pub null: bool,

    // --- Compression ---

    #[command(flatten)]
    pub compression: CompressionFlags,

    /// Compression level. Allowed range depends on filter: gzip/bzip2 1–9, xz 0–9, zstd 1–19.
    #[arg(long = "level", value_name = "N")]
    pub level: Option<u32>,

    /// xz `--extreme` / `-e` (OR into LZMA preset). Only valid with xz.
    #[arg(long = "xz-extreme")]
    pub xz_extreme: bool,

    // INFO: Rust does not expose bzip-small option.,

    /// Cap xz encoder RAM (bytes, MiB, GiB, or % of RAM). Like `xz --memlimit-compress`.
    #[arg(long = "memlimit-compress", value_name = "LIMIT")]
    pub memlimit_compress: Option<String>,

    // --- Selection / walk ---

    /// Do not descend into directories.
    #[arg(long = "no-recursion", default_value_t = false)]
    pub no_recursion: bool,

    /// Follow symlinks; archive the files they point to (GNU tar `-h`).
    #[arg(long = "dereference", default_value_t = false)]
    pub dereference: bool,

    /// Stay on one filesystem when walking input trees.
    #[arg(long = "one-file-system", default_value_t = false)]
    pub one_file_system: bool,

    /// Do not strip leading `/` from stored names (policy for absolute paths).
    #[arg(short = 'P', long = "absolute-names", default_value_t = false)]
    pub absolute_names: bool,

    /// Do not coalesce same (inode, device) hard links in hash/dedup.
    #[arg(long = "no-hardlink-detection", default_value_t = false)]
    pub no_hardlink_detection: bool,

    // --- Filter ---

    /// Exclude paths matching regex PATTERN (repeatable).
    #[arg(long = "exclude", value_name = "PATTERN", action = ArgAction::Append)]
    pub exclude: Vec<String>,

    /// Read exclude regex patterns from FILE (repeatable).
    #[arg(
        short = 'X',
        long = "exclude-from",
        value_name = "FILE",
        action = ArgAction::Append
    )]
    pub exclude_from: Vec<PathBuf>,

    /// Include only paths matching regex PATTERN (repeatable).
    #[arg(long = "include", value_name = "PATTERN", action = ArgAction::Append)]
    pub include: Vec<String>,

    /// Read include regex patterns from FILE (repeatable).
    #[arg(long = "include-from", value_name = "FILE", action = ArgAction::Append)]
    pub include_from: Vec<PathBuf>,

    /// Exclude version control system directories (not implemented yet).
    #[arg(long = "exclude-vcs", default_value_t = false)]
    pub exclude_vcs: bool,

    /// Read VCS ignore files for exclusions (not implemented yet).
    #[arg(long = "exclude-vcs-ignores", default_value_t = false)]
    pub exclude_vcs_ignores: bool,

    /// Patterns match from the start of the relative path.
    #[arg(long = "anchored", default_value_t = false, action = ArgAction::SetTrue)]
    #[arg(long = "no-anchored", action = ArgAction::SetFalse)]
    pub anchored: bool,

    /// Case-insensitive pattern matching.
    #[arg(long = "ignore-case", default_value_t = false, action = ArgAction::SetTrue)]
    #[arg(long = "no-ignore-case", action = ArgAction::SetFalse)]
    pub ignore_case: bool,

    // --- Attributes ---

    /// Capture POSIX ACLs (default: on).
    #[arg(long = "acls", default_value_t = true, action = ArgAction::SetTrue)]
    #[arg(long = "no-acls", action = ArgAction::SetFalse)]
    pub acls: bool,

    /// Capture extended attributes (default: on).
    #[arg(long = "xattrs", default_value_t = true, action = ArgAction::SetTrue)]
    #[arg(long = "no-xattrs", action = ArgAction::SetFalse)]
    pub xattrs: bool,

    /// Capture SELinux contexts (default: on).
    #[arg(long = "selinux", default_value_t = true, action = ArgAction::SetTrue)]
    #[arg(long = "no-selinux", action = ArgAction::SetFalse)]
    pub selinux: bool,

    /// Force owner for archived members: `NAME`, `UID`, or `NAME:UID` (GNU tar).
    /// Stored as archive policy (meta); applied on extract.
    #[arg(long = "owner", value_name = "NAME[:UID]")]
    pub owner: Option<String>,

    /// Owner translation map file (GNU tar `--owner-map`).
    #[arg(long = "owner-map", value_name = "FILE")]
    pub owner_map: Option<PathBuf>,

    /// Force group for archived members: `NAME`, `GID`, or `NAME:GID` (GNU tar).
    /// Stored as archive policy (meta); applied on extract.
    #[arg(long = "group", value_name = "NAME[:GID]")]
    pub group: Option<String>,

    /// Group translation map file (GNU tar `--group-map`).
    #[arg(long = "group-map", value_name = "FILE")]
    pub group_map: Option<PathBuf>,

    // --- Sparse ---

    /// Run the sparsify phase (copy with seeks using `--page-size` / `--min-pages`).
    #[arg(long = "sparsify", default_value_t = false)]
    pub sparsify: bool,

    /// Page size in bytes for hash zero-page counting and sparsify.
    #[arg(long = "page-size", value_name = "BYTES", default_value_t = 4096)]
    pub page_size: usize,

    /// Minimum empty-page count before sparsify treats a file as worth rewriting.
    #[arg(long = "min-pages", value_name = "PAGES")]
    pub min_pages: Option<u64>,

    // --- Pipeline / runtime ---

    /// Maximum concurrent workers (rayon pools and xz threads).
    #[arg(long = "jobs", value_name = "N")]
    pub jobs: Option<usize>,

    /// Wipe stage work and restart from inventory.
    #[arg(long = "fresh")]
    pub fresh: bool,

    /// After success, keep a timestamped copy of snapshot.sqlite next to the archive.
    #[arg(long = "keep-db")]
    pub keep_db: bool,

    /// After success, keep the `{stem}.astage` work directory.
    #[arg(long = "keep-stage")]
    pub keep_stage: bool,

    /// Run through STAGE then exit cleanly (state saved). STAGE: inventory, hash,
    /// filter, dedup, sparsify, stage, archive/tar, cleanup (alias scan for inventory).
    #[arg(long = "exit-after-stage", value_name = "STAGE", value_enum)]
    pub exit_after_stage: Option<ExitAfterStageArg>,

    /// Abort the run on the first hard failure (instead of soft-continuing where possible).
    #[arg(long = "fail-fast", default_value_t = false)]
    pub fail_fast: bool,

    /// Record per-file error messages and continue instead of failing the run.
    #[arg(long = "no-errors", default_value_t = false)]
    pub no_errors: bool,

    /// Apply include/exclude filters before the hash phase.
    #[arg(long = "eager-filter", default_value_t = false)]
    pub eager_filter: bool,

    /// Skip the deduplication phase.
    #[arg(long = "no-dedup", default_value_t = false)]
    pub no_dedup: bool,

    /// Stage/archive files that failed to obtain a SHA-1.
    #[arg(long = "retry-missing-sha", default_value_t = false)]
    pub retry_missing_sha: bool,
}

#[derive(Debug, Args, Default)]
pub struct CompressionFlags {
    /// Use archive suffix to pick the compression filter.
    #[arg(short = 'a', long = "auto-compress", group = "compress_filter")]
    pub auto_compress: bool,

    /// Do not infer compression from the archive suffix.
    #[arg(long = "no-auto-compress")]
    pub no_auto_compress: bool,

    #[arg(short = 'z', long = "gzip", group = "compress_filter")]
    pub gzip: bool,

    #[arg(short = 'j', long = "bzip2", group = "compress_filter")]
    pub bzip2: bool,

    #[arg(short = 'J', long = "xz", group = "compress_filter")]
    pub xz: bool,

    #[arg(long = "zstd", group = "compress_filter")]
    pub zstd: bool,
}

#[derive(Debug, Args)]
pub struct ExtractArgs {
    #[arg(short = 'f')]
    pub archive: PathBuf,

    /// Extract files relative to this directory (like GNU tar -C).
    #[arg(short = 'C', value_name = "DIR")]
    pub output_dir: PathBuf,

    /// Restore saved uid/gid on extracted files (best effort; may require root).
    #[arg(long)]
    pub restore_owner: bool,

    /// Resume from incomplete extract work (error if nothing to resume).
    #[arg(long, conflicts_with = "fresh")]
    pub resume: bool,

    /// Wipe extract stage work and start over.
    #[arg(long, conflicts_with = "resume")]
    pub fresh: bool,

    /// After success, keep a timestamped copy of snapshot.sqlite in the output directory.
    #[arg(long = "keep-db")]
    pub keep_db: bool,

    /// After success, keep the `{stem}.estage` work directory.
    #[arg(long = "keep-stage")]
    pub keep_stage: bool,
}

/// Pipeline stop point for `--exit-after-stage`.
///
/// Names follow archive file-phase progression (plus `cleanup`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ExitAfterStageArg {
    /// Walk the input tree / capture metadata (`inventoried`).
    #[value(alias = "scan")]
    Inventory,
    /// Content hash + sparse-page scan (`hashed`).
    Hash,
    /// Include/exclude selection (`filtered`).
    Filter,
    /// Canonical / duplicate resolution (`deduped`).
    Dedup,
    /// Sparse materialization (`sparsified`).
    Sparsify,
    /// Symlink canonical files into the work directory (`staged`).
    #[value(alias = "symlink")]
    Stage,
    /// Write the compressed tar archive (`archived`).
    #[value(alias = "tar")]
    Archive,
    /// Full pipeline then clean the work directory (honours `--keep-db` / `--keep-stage`).
    Cleanup,
}
