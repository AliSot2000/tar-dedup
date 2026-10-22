use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::cli::{ArchiveArgs, CompressionFlags};
use crate::compression::resolve_xz_threads;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionSettings {
    pub format: CompressionFormat,
    /// Compression level for the active filter (ignored when `format` is None).
    pub level: u32,
    /// xz `-e` / `--extreme` (preset extreme bit).
    pub xz_extreme: bool,
    /// Max RAM for xz MT encoder (`None` = no limit, like default `xz`).
    pub memlimit_compress: Option<u64>,
}

impl Default for CompressionSettings {
    fn default() -> Self {
        Self {
            format: CompressionFormat::None,
            level: 0,
            xz_extreme: false,
            memlimit_compress: None,
        }
    }
}

impl CompressionSettings {
    pub fn validate_aux(&self, jobs: usize) -> Result<()> {
        let _ = resolve_xz_threads(jobs, self.memlimit_compress, self.level)?;
        Ok(())
    }
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

    // No filter flag: the archive suffix decides (extension inference; unknown
    // or bare suffix means no compression).
    Ok(infer_compression_from_suffix(archive_path))
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

// ---------------------------------------------------------------------------------------------------------------------
// TEST — compression config validation (feature `test-compression`)
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(all(test, feature = "test-compression"))]
mod tests {
    use super::*;
    use clap::Parser;
    use crate::cli::{Cli, Command};

    fn flags_of(
        xz: bool, gzip: bool, bzip2: bool, zstd: bool) -> CompressionFlags {
        CompressionFlags { xz, gzip, bzip2, zstd }
    }

    fn archive_args(suffix: &str, extra: &[&str]) -> ArchiveArgs {
        let mut argv = vec!["tar-dedup", "archive", "-f", suffix];
        argv.extend_from_slice(extra);
        match Cli::try_parse_from(&argv).expect("parse args").command {
            Command::Archive(a) => a,
            _ => panic!("expected archive subcommand"),
        }
    }

    #[test]
    fn level_range_and_defaults() {
        assert_eq!(CompressionFormat::Xz.level_range(), Some((0, 9)));
        assert_eq!(CompressionFormat::Gz.level_range(), Some((1, 9)));
        assert_eq!(CompressionFormat::Bz2.level_range(), Some((1, 9)));
        assert_eq!(CompressionFormat::Zstd.level_range(), Some((1, 19)));
        assert_eq!(CompressionFormat::None.level_range(), None);
        for format in [CompressionFormat::Xz, CompressionFormat::Gz, CompressionFormat::Bz2, CompressionFormat::Zstd] {
            assert_eq!(
                format.default_level(),
                Some(format.level_range().unwrap().1),
                "{format:?} default must be the max level"
            );
            assert!(format.does_compress());
        }
        assert_eq!(CompressionFormat::None.default_level(), None);
        assert!(!CompressionFormat::None.does_compress());
    }

    #[test]
    fn infer_suffix_cases() {
        use std::path::Path;
        let cases: [(&str, CompressionFormat); 13] = [
            ("a.tar.xz", CompressionFormat::Xz),
            ("a.txz", CompressionFormat::Xz),
            ("a.tar.gz", CompressionFormat::Gz),
            ("a.tgz", CompressionFormat::Gz),
            ("a.tar.bz2", CompressionFormat::Bz2),
            ("a.tbz2", CompressionFormat::Bz2),
            ("a.tbz", CompressionFormat::Bz2),
            ("a.tar.zst", CompressionFormat::Zstd),
            ("a.tzst", CompressionFormat::Zstd),
            ("A.X.TAR.XZ", CompressionFormat::Xz),
            ("a.tar", CompressionFormat::None),
            ("a.txt", CompressionFormat::None),
            ("archive", CompressionFormat::None),
        ];
        for (name, expected) in cases {
            assert_eq!(
                infer_compression_from_suffix(Path::new(name)),
                expected,
                "suffix inference for {name}"
            );
        }
    }

    #[test]
    fn resolve_compression_flags_and_suffix() {
        use std::path::Path;
        // Flag beats suffix.
        assert_eq!(
            resolve_compression(&flags_of(false, true, false, false), Path::new("a.tar.xz"))
                .expect("gzip flag won"),
            CompressionFormat::Gz
        );
        assert_eq!(
            resolve_compression(&flags_of(true, false, false, false), Path::new("a.tar.gz"))
                .expect("xz flag won"),
            CompressionFormat::Xz
        );
        assert_eq!(
            resolve_compression(&flags_of(false, false, false, true), Path::new("a.tar.bz2"))
                .expect("zstd flag won"),
            CompressionFormat::Zstd
        );
        // No flag: suffix decides; bare suffix means None.
        assert_eq!(
            resolve_compression(&CompressionFlags::default(), Path::new("a.tar.xz")).expect("xz"),
            CompressionFormat::Xz
        );
        assert_eq!(
            resolve_compression(&CompressionFlags::default(), Path::new("a.tar")).expect("none"),
            CompressionFormat::None
        );
        // Conflicting filter flags.
        assert!(matches!(
            resolve_compression(&flags_of(true, true, false, false), Path::new("a.tar")),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn from_archive_args_defaults_and_boundaries() {
        let args = archive_args("a.tar.xz", &[]);
        let s = CompressionSettings::from_archive_args(CompressionFormat::Xz, &args)
            .expect("xz defaults");
        assert_eq!(s.format, CompressionFormat::Xz);
        assert_eq!(s.level, 9);
        assert!(!s.xz_extreme);
        assert_eq!(s.memlimit_compress, None);

        // Boundary levels accepted per format.
        for (format, level) in [
            (CompressionFormat::Xz, 0),
            (CompressionFormat::Xz, 9),
            (CompressionFormat::Gz, 1),
            (CompressionFormat::Gz, 9),
            (CompressionFormat::Bz2, 9),
            (CompressionFormat::Zstd, 19),
        ] {
            let args = archive_args("a.tar", &["--level", &level.to_string()]);
            let s = CompressionSettings::from_archive_args(format, &args)
                .expect("boundary level accepted");
            assert_eq!(s.level, level);
        }

        // xz extreme is only meaningful with xz — allowed and sticky.
        let args = archive_args("a.tar", &["--xz-extreme"]);
        let s = CompressionSettings::from_archive_args(CompressionFormat::Xz, &args)
            .expect("xz extreme accepted");
        assert!(s.xz_extreme);
    }

    #[test]
    fn from_archive_args_rejects_bad_combos() {
        // Level out of range per format.
        for (format, level) in [
            (CompressionFormat::Xz, 10),
            (CompressionFormat::Gz, 0),
            (CompressionFormat::Bz2, 10),
            (CompressionFormat::Zstd, 20),
        ] {
            let args = archive_args("a.tar", &["--level", &level.to_string()]);
            assert!(
                matches!(
                    CompressionSettings::from_archive_args(format, &args),
                    Err(Error::Config(_))
                ),
                "{format:?} level {level} must be rejected"
            );
        }

        // --level without a filter (uncompressed archive).
        let args = archive_args("a.tar", &["--level", "5"]);
        assert!(matches!(
            CompressionSettings::from_archive_args(CompressionFormat::None, &args),
            Err(Error::Config(_))
        ));

        // --xz-extreme / --memlimit-compress on a non-xz filter.
        for extra in [vec!["--xz-extreme"], vec!["--memlimit-compress", "64MiB"]] {
            let args = archive_args("a.tar", &extra);
            assert!(
                matches!(
                    CompressionSettings::from_archive_args(CompressionFormat::Gz, &args),
                    Err(Error::Config(_))
                ),
                "non-xz must reject {extra:?}"
            );
        }

        // memlimit parses to bytes on xz.
        let args = archive_args("a.tar", &["--memlimit-compress", "64MiB"]);
        let s = CompressionSettings::from_archive_args(CompressionFormat::Xz, &args)
            .expect("xz memlimit accepted");
        assert_eq!(s.memlimit_compress, Some(64 * 1024 * 1024));
    }
}
