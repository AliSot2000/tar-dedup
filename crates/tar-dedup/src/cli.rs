use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

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
    /// Archive path (relative paths are resolved from the current directory).
    #[arg(short = 'f')]
    pub archive: PathBuf,

    /// Input directory to snapshot.
    #[arg(short = 'i')]
    pub input: PathBuf,

    /// Working directory for sqlite/staging (defaults to `{stem}.astage` next to the archive).
    #[arg(short = 'C')]
    pub work_dir: Option<PathBuf>,

    #[command(flatten)]
    pub compression: CompressionFlags,

    /// Compression level. Allowed range depends on filter: gzip/bzip2 1–9, xz 0–9, zstd 1–19.
    #[arg(long = "level", value_name = "N")]
    pub level: Option<u32>,

    /// xz `--extreme` / `-e` (OR into LZMA preset). Only valid with xz.
    #[arg(long = "xz-extreme")]
    pub xz_extreme: bool,

    /// bzip2 `-s` (small / 100k blocks). Only valid with bzip2.
    #[arg(long = "bzip-small")]
    pub bzip_small: bool,

    /// Maximum concurrent workers.
    #[arg(long = "jobs", value_name = "N")]
    pub jobs: Option<usize>,

    /// Resume from incomplete work in the stage directory (error if nothing to resume).
    #[arg(long, conflicts_with = "fresh")]
    pub resume: bool,

    /// Wipe stage work and restart from inventory.
    #[arg(long, conflicts_with = "resume")]
    pub fresh: bool,

    /// After success, keep a timestamped copy of snapshot.sqlite next to the archive.
    #[arg(long = "keep-db")]
    pub keep_db: bool,

    /// After success, keep the `{stem}.astage` work directory.
    #[arg(long = "keep-stage")]
    pub keep_stage: bool,

    /// Run through STAGE then exit cleanly (state saved). STAGE: scan, hash, dedup,
    /// sparsify, stage, tar, cleanup (and aliases inventory, archive).
    #[arg(long = "exit-after-stage", value_name = "STAGE", value_enum)]
    pub exit_after_stage: Option<ExitAfterStageArg>,

    /// Cap xz encoder RAM (bytes, MiB, GiB, or % of RAM). Like `xz --memlimit-compress`.
    #[arg(long = "memlimit-compress", value_name = "LIMIT")]
    pub memlimit_compress: Option<String>,

    /// Page size in bytes for hash zero-page counting and sparsify (default 4096).
    #[arg(long = "page-size", value_name = "BYTES", default_value_t = 4096)]
    pub page_size: usize,

    // TODO: Added default of 0
    /// Minimum empty-page count before sparsify treats a file as worth rewriting.
    #[arg(long = "min-pages", value_name = "PAGES")]
    pub min_pages: Option<u64>,
}

#[derive(Debug, Args, Default)]
pub struct CompressionFlags {
    /// Use archive suffix to pick the compression filter.
    #[arg(short = 'a', long = "auto-compress", group = "compress_filter")]
    pub auto_compress: bool,

    #[arg(short = 'z', long = "gzip", group = "compress_filter")]
    pub gzip: bool,

    #[arg(short = 'j', long = "bzip2", group = "compress_filter")]
    pub bzip2: bool,

    #[arg(short = 'J', long = "xz", group = "compress_filter")]
    pub xz: bool,

    #[arg(long = "zstd", group = "compress_filter")]
    pub zstd: bool,

    /// Do not infer compression from the archive suffix.
    #[arg(long = "no-auto-compress")]
    pub no_auto_compress: bool,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lower")]
pub enum ExitAfterStageArg {
    /// Walk the input tree (inventory).
    #[value(alias = "inventory")]
    Scan,
    Hash,
    Dedup,
    Sparsify,
    /// Symlink canonical files into the `{stem}.astage` work directory.
    #[value(alias = "symlink")]
    Stage,
    /// Write the compressed tar archive.
    #[value(alias = "archive")]
    Tar,
    /// Full pipeline then clean the work directory (honours `--keep-db` / `--keep-stage`).
    Cleanup,
}
