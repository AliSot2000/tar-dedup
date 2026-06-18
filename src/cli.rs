use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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
    /// Restore an archive (not yet implemented).
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

    /// GNU tar alias for xz.
    #[arg(long = "lzma", group = "compress_filter")]
    pub lzma: bool,

    #[arg(long = "lzip", group = "compress_filter")]
    pub lzip: bool,

    #[arg(long = "lzop", group = "compress_filter")]
    pub lzop: bool,

    #[arg(short = 'Z', long = "compress", group = "compress_filter")]
    pub compress: bool,

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

    /// Directory to restore into (required for extract).
    #[arg(short = 'C')]
    pub output_dir: PathBuf,

    #[arg(long)]
    pub restore_owner: bool,
}
