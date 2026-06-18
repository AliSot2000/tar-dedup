use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cli::{ArchiveArgs, CompressionFlags, ExtractArgs};
use crate::error::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompressionFormat {
    Xz,
    Gz,
    Bz2,
    Zstd,
    None,
}

impl CompressionFormat {
    pub fn allows_resume(self) -> bool {
        true
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub archive_path: PathBuf,
    pub input_dir: PathBuf,
    pub work_dir: PathBuf,
    pub compression: CompressionFormat,
    pub jobs: usize,
    pub resume: bool,
}

impl Config {
    pub fn from_archive_args(args: &ArchiveArgs) -> Result<Self> {
        validate_dir(&args.input, "input directory")?;

        let archive_path = resolve_user_path(&args.archive)?;
        if let Some(parent) = archive_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        let work_dir = match &args.work_dir {
            Some(path) => resolve_user_path(path)?,
            None => default_work_dir(&archive_path),
        };
        std::fs::create_dir_all(&work_dir).map_err(|e| Error::io(&work_dir, e))?;

        let compression = resolve_compression(&args.compression, &archive_path)?;
        let jobs = args.jobs.unwrap_or_else(num_cpus::get);

        Ok(Self {
            archive_path,
            input_dir: args.input.clone(),
            work_dir,
            compression,
            jobs,
            resume: args.resume,
        })
    }

    pub fn from_extract_args(args: &ExtractArgs) -> Result<Self> {
        let archive_path = resolve_user_path(&args.archive)?;
        let output_dir = resolve_user_path(&args.output_dir)?;
        std::fs::create_dir_all(&output_dir).map_err(|e| Error::io(&output_dir, e))?;

        Ok(Self {
            archive_path,
            input_dir: PathBuf::new(),
            work_dir: output_dir,
            compression: CompressionFormat::None,
            jobs: 1,
            resume: false,
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.work_dir.join("snapshot.sqlite")
    }

    pub fn stage_dir(&self) -> PathBuf {
        self.work_dir.join("stage")
    }
}

pub fn resolve_compression(flags: &CompressionFlags, archive_path: &Path) -> Result<CompressionFormat> {
    if let Some(prog) = &flags.use_compress_program {
        return Err(Error::Config(format!(
            "external compress program is not implemented yet: {}",
            prog.display()
        )));
    }

    let mut chosen = None;
    let mut pick = |name: &str, format: CompressionFormat| -> Result<()> {
        if chosen.is_some() {
            return Err(Error::Config(format!(
                "compression filter '{name}' conflicts with another compression flag"
            )));
        }
        chosen = Some(format);
        Ok(())
    };

    if flags.xz || flags.lzma {
        pick("xz/lzma", CompressionFormat::Xz)?;
    }
    if flags.gzip {
        pick("gzip", CompressionFormat::Gz)?;
    }
    if flags.bzip2 {
        pick("bzip2", CompressionFormat::Bz2)?;
    }
    if flags.zstd {
        pick("zstd", CompressionFormat::Zstd)?;
    }
    if flags.lzip {
        return Err(Error::Config(
            "lzip is not implemented yet; use -J/--xz, -z/--gzip, -j/--bzip2, or --zstd".into(),
        ));
    }
    if flags.lzop {
        return Err(Error::Config(
            "lzop is not implemented yet; use -J/--xz, -z/--gzip, -j/--bzip2, or --zstd".into(),
        ));
    }
    if flags.compress {
        return Err(Error::Config(
            "unix compress is not implemented yet; use -J/--xz, -z/--gzip, -j/--bzip2, or --zstd".into(),
        ));
    }

    if let Some(format) = chosen {
        return Ok(format);
    }

    if flags.auto_compress || !flags.no_auto_compress {
        return Ok(infer_compression_from_suffix(archive_path));
    }

    Ok(CompressionFormat::None)
}

fn infer_compression_from_suffix(path: &Path) -> CompressionFormat {
    let name = path.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        CompressionFormat::Xz
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        CompressionFormat::Gz
    } else if name.ends_with(".tar.bz2") || name.ends_with(".tbz2") || name.ends_with(".tbz") {
        CompressionFormat::Bz2
    } else if name.ends_with(".tar.zst") || name.ends_with(".tzst") {
        CompressionFormat::Zstd
    } else {
        CompressionFormat::None
    }
}

pub fn resolve_user_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        let cwd = std::env::current_dir().map_err(Error::from)?;
        Ok(cwd.join(path))
    }
}

fn default_work_dir(archive_path: &Path) -> PathBuf {
    let parent = archive_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = archive_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".into());
    parent.join(format!(".{name}.work"))
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
        return Err(Error::Config(format!(
            "{label} does not exist or is not a directory: {}",
            path.display()
        )));
    }
    Ok(())
}
