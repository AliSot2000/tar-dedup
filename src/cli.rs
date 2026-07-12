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

    /// Working directory for sqlite/staging (defaults to `.NAME.work` next to the archive).
    #[arg(short = 'C')]
    pub work_dir: Option<PathBuf>,

    #[command(flatten)]
    pub compression: CompressionFlags,

    /// Maximum concurrent workers.
    #[arg(long = "jobs", value_name = "N")]
    pub jobs: Option<usize>,

    /// Resume from existing state in the work directory (auto-detected if omitted).
    #[arg(long)]
    pub resume: bool,

    /// Ignore saved state and restart from inventory.
    #[arg(long, conflicts_with = "resume")]
    pub fresh: bool,

    /// Keep work-dir snapshot.sqlite and stage/ after a successful archive.
    #[arg(long)]
    pub keep_stage: bool,

    /// Run through STAGE then exit cleanly (state saved). STAGE: scan, hash, dedup,
    /// stage, tar, cleanup (and aliases inventory, archive).
    #[arg(long = "exit-after-stage", value_name = "STAGE", value_enum)]
    pub exit_after_stage: Option<ExitAfterStageArg>,

    /// Cap xz encoder RAM (bytes, MiB, GiB, or % of RAM). Like `xz --memlimit-compress`.
    #[arg(long = "memlimit-compress", value_name = "LIMIT")]
    pub memlimit_compress: Option<String>,
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
    
    // TODO: Shell out needs to be defined.
    /// Filter through PROG (must accept -d). Not implemented yet.
    #[arg(short = 'I', long = "use-compress-program", value_name = "PROG")]
    pub use_compress_program: Option<PathBuf>,

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

    /// Ignore saved extract state and start over.
    #[arg(long)]
    pub fresh: bool,
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
    /// Symlink canonical files into the work-dir stage/.
    #[value(alias = "symlink")]
    Stage,
    /// Write the compressed tar archive.
    #[value(alias = "archive")]
    Tar,
    /// Full pipeline then remove the work directory (unless `--keep-stage`).
    Cleanup,
}
