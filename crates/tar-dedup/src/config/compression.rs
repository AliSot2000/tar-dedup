use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::cli::{ArchiveArgs, CompressionFlags};
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
    pub fn does_compress(self) -> bool {
        match self {
            Self::Xz | Self::Gz | Self::Bz2 | Self::Zstd => true,
            Self::None => false,
        }
    }

    /// Allowed `--level` range for this filter (`None` if uncompressed).
    pub fn level_range(self) -> Option<(u32, u32)> {
        match self {
            Self::Gz | Self::Bz2 => Some((1, 9)),
            Self::Xz => Some((0, 9)),
            Self::Zstd => Some((1, 19)),
            Self::None => None,
        }
    }

    /// Default level when `--level` is omitted (previous hard-coded maxima).
    pub fn default_level(self) -> Option<u32> {
        self.level_range().map(|(_, max)| max)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Xz => "xz",
            Self::Gz => "gzip",
            Self::Bz2 => "bzip2",
            Self::Zstd => "zstd",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompressionSettings {
    pub format: CompressionFormat,
    /// Compression level for the active filter (ignored when `format` is None).
    pub level: u32,
    /// xz `-e` / `--extreme` (preset extreme bit).
    pub xz_extreme: bool,
    /// Max RAM for xz MT encoder (`None` = no limit, like default `xz`).
    pub memlimit_compress: Option<u64>,
}

impl CompressionSettings {
    /// Validate CLI compression options and build settings for archive creation.
    pub fn from_archive_args(format: CompressionFormat, args: &ArchiveArgs) -> Result<Self> {
        let (level, xz_extreme) = Self::resolve_compress_options(format, args)?;
        let memlimit_compress = args
            .memlimit_compress
            .as_deref()
            .map(super::parse_memlimit)
            .transpose()?;
        if memlimit_compress.is_some() && format != CompressionFormat::Xz {
            return Err(Error::Config(
                "--memlimit-compress is only valid with xz compression".into(),
            ));
        }
        Ok(Self {
            format,
            level,
            xz_extreme,
            memlimit_compress,
        })
    }

    fn resolve_compress_options(format: CompressionFormat, args: &ArchiveArgs) -> Result<(u32, bool)> {
        if args.xz_extreme && format != CompressionFormat::Xz {
            return Err(Error::Config(
                "--xz-extreme is only valid with xz compression (-J / .tar.xz)".into(),
            ));
        }
        if format == CompressionFormat::None {
            if args.level.is_some() {
                return Err(Error::Config(
                    "--level requires a compression filter (archive is uncompressed)".into(),
                ));
            }
            return Ok((0, false));
        }

        let (min, max) = format
            .level_range()
            .expect("compressing format has a level range");
        let level = args.level.unwrap_or_else(|| format.default_level().unwrap());
        if level < min || level > max {
            return Err(Error::Config(format!(
                "--level {level} is out of range for {} (allowed {min}–{max})",
                format.as_str()
            )));
        }
        Ok((level, args.xz_extreme))
    }
}

pub fn resolve_compression(flags: &CompressionFlags, archive_path: &Path) -> Result<CompressionFormat> {
    let mut chosen: Option<CompressionFormat> = None;
    let mut pick = |name: &str, format: CompressionFormat| -> Result<()> {
        if chosen.is_some() {
            let chosen_name = chosen.unwrap().as_str();
            return Err(Error::Config(format!(
                "compression filter '{name}' conflicts with {chosen_name}"
            )));
        }
        chosen = Some(format);
        Ok(())
    };

    if flags.xz {
        pick("xz", CompressionFormat::Xz)?;
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

    if let Some(format) = chosen {
        return Ok(format);
    }

    if flags.auto_compress || !flags.no_auto_compress {
        return Ok(infer_compression_from_suffix(archive_path));
    }

    if flags.auto_compress && flags.no_auto_compress {
        return Err(Error::Config(
            "--no-auto-compress and --auto-compress cannot be passed at the same time.".to_string(),
        ));
    }

    Ok(CompressionFormat::None)
}

pub fn infer_compression_from_suffix(path: &Path) -> CompressionFormat {
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
