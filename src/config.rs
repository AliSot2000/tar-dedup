use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cli::{ArchiveArgs, ExtractArgs};
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressionFormat {
    Xz,
    Gz,
    Bz2,
    None,
}

impl CompressionFormat {
    pub fn file_extension(self) -> &'static str {
        match self {
            Self::Xz => "xz",
            Self::Gz => "gz",
            Self::Bz2 => "bz2",
            Self::None => "tar",
        }
    }

    pub fn allows_resume(self) -> bool {
        !matches!(self, Self::Gz)
    }

    pub fn is_single_shot(self) -> bool {
        matches!(self, Self::Gz)
    }
}

/// User-facing settings derived from CLI flags.
#[derive(Debug, Clone)]
pub struct Config {
    pub archive_path: PathBuf,
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
    pub compression: CompressionFormat,
    pub jobs: usize,
    pub resume: bool,
}

impl Config {
    pub fn from_archive_args(args: &ArchiveArgs) -> Result<Self> {
        validate_dir(&args.input, "input directory")?;
        std::fs::create_dir_all(&args.output_dir).map_err(|e| Error::io(&args.output_dir, e))?;

        let jobs = args.jobs.unwrap_or_else(num_cpus::get);

        Ok(Self {
            archive_path: args.archive.clone(),
            input_dir: args.input.clone(),
            output_dir: args.output_dir.clone(),
            compression: args.compression.into(),
            jobs,
            resume: args.resume,
        })
    }

    pub fn from_extract_args(args: &ExtractArgs) -> Result<Self> {
        std::fs::create_dir_all(&args.output_dir).map_err(|e| Error::io(&args.output_dir, e))?;

        Ok(Self {
            archive_path: args.archive.clone(),
            input_dir: PathBuf::new(),
            output_dir: args.output_dir.clone(),
            compression: CompressionFormat::None,
            jobs: 1,
            resume: false,
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.output_dir.join("snapshot.sqlite")
    }

    pub fn stage_dir(&self) -> PathBuf {
        self.output_dir.join("stage")
    }

    pub fn runtime_config_path(&self) -> PathBuf {
        self.output_dir.join("runtime.json")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipelinePhase {
    Inventory,
    Hash,
    Dedup,
    Stage,
    Archive,
    Done,
}

impl PipelinePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inventory => "inventory",
            Self::Hash => "hash",
            Self::Dedup => "dedup",
            Self::Stage => "stage",
            Self::Archive => "archive",
            Self::Done => "done",
        }
    }

    pub fn next(self) -> Option<Self> {
        match self {
            Self::Inventory => Some(Self::Hash),
            Self::Hash => Some(Self::Dedup),
            Self::Dedup => Some(Self::Stage),
            Self::Stage => Some(Self::Archive),
            Self::Archive => Some(Self::Done),
            Self::Done => None,
        }
    }
}

/// Persisted pipeline cursor (not the file tree).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeState {
    pub snapshot_taken_at: DateTime<Utc>,
    pub phase: PipelinePhase,
    pub max_workers: usize,
}

impl RuntimeState {
    pub fn new(max_workers: usize) -> Self {
        Self {
            snapshot_taken_at: Utc::now(),
            phase: PipelinePhase::Inventory,
            max_workers,
        }
    }
}

fn validate_dir(path: &Path, label: &str) -> Result<()> {
    if !path.is_dir() {
        return Err(Error::Config(format!("{label} does not exist or is not a directory: {}", path.display())));
    }
    Ok(())
}
