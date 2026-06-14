use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::config::CompressionFormat;

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

#[derive(Debug, clap::Args)]
pub struct ArchiveArgs {
    /// Output archive path (e.g. snapshot.tar.xz).
    #[arg(short = 'f')]
    pub archive: PathBuf,

    /// Input directory to snapshot.
    #[arg(short = 'i')]
    pub input: PathBuf,

    /// Working/output directory (db, staging, temp).
    #[arg(short = 'C')]
    pub output_dir: PathBuf,

    /// Compression format for tar payload sessions.
    #[arg(long, value_enum, default_value_t = CompressionFormatCli::Xz)]
    pub compression: CompressionFormatCli,

    /// Maximum concurrent workers (hash/compress/etc.).
    #[arg(short = 'j', long = "jobs")]
    pub jobs: Option<usize>,

    /// Resume from existing state in the output directory.
    #[arg(long)]
    pub resume: bool,
}

#[derive(Debug, clap::Args)]
pub struct ExtractArgs {
    #[arg(short = 'f')]
    pub archive: PathBuf,

    #[arg(short = 'C')]
    pub output_dir: PathBuf,

    #[arg(long)]
    pub restore_owner: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CompressionFormatCli {
    Xz,
    Gz,
    Bz2,
    None,
}

impl From<CompressionFormatCli> for CompressionFormat {
    fn from(value: CompressionFormatCli) -> Self {
        match value {
            CompressionFormatCli::Xz => CompressionFormat::Xz,
            CompressionFormatCli::Gz => CompressionFormat::Gz,
            CompressionFormatCli::Bz2 => CompressionFormat::Bz2,
            CompressionFormatCli::None => CompressionFormat::None,
        }
    }
}
