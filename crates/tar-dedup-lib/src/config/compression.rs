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

/// Optional `xz` LZMA2 tuning, parsed from `--xz-advanced <JSON>`.
///
/// Every field is optional; a field present overrides the equivalent parameter of
/// the `--level` preset (the preset is evaluated first, then these overrides are
/// applied). The struct intentionally carries no BCJ/delta options — those are
/// architecture-niche and out of scope; use the bypass product + another tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct XzAdvancedOptions {
    /// LZMA2 preset level 0–9 (overrides `--level`).
    pub level: Option<u32>,
    /// xz `-e` / `--extreme`.
    pub extreme: Option<bool>,
    /// LZMA2 dictionary size (bytes or `"64MiB"`, 16 KiB – 1.5 GiB, `u32` clamp).
    /// Larger windows capture cross-file redundancy on many small files.
    #[serde(default, deserialize_with = "de_opt_size")]
    pub dict: Option<u64>,
    /// MT encoder block size (bytes or `"64MiB"`; `0` = liblzma default). The xz2
    /// crate exposes block size but not a custom block list.
    #[serde(default, deserialize_with = "de_opt_size")]
    pub block_size: Option<u64>,
    /// Encoder threads (overrides `--jobs`).
    #[serde(default)]
    pub threads: Option<u32>,
    /// Number of literal context bits (0–4).
    pub lc: Option<u32>,
    /// Number of literal position bits (0–4).
    pub lp: Option<u32>,
    /// Number of position bits (0–4).
    pub pb: Option<u32>,
    /// Compression mode: `fast` or `normal`.
    pub mode: Option<XzMode>,
    /// Nice len for the match finder (2–273, default preset).
    pub nice: Option<u32>,
    /// Match finder: `hc3`, `hc4`, `bt2`, `bt3`, or `bt4`.
    pub mf: Option<XzMatchFinder>,
    /// Search depth limit for match finders (0 = preset default).
    pub depth: Option<u32>,
    /// Integrity check: `none`, `crc32`, `crc64`, or `sha256`.
    pub check: Option<XzCheck>,
    /// Max RAM for the encoder (bytes or `"64MiB"`, like `xz --memlimit-compress`).
    #[serde(default, deserialize_with = "de_opt_size")]
    pub memlimit: Option<u64>,
}

/// `mode` values for [`XzAdvancedOptions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum XzMode {
    Fast,
    Normal,
}

/// `mf` values for [`XzAdvancedOptions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum XzMatchFinder {
    Hc3,
    Hc4,
    Bt2,
    Bt3,
    Bt4,
}

/// `check` values for [`XzAdvancedOptions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum XzCheck {
    None,
    Crc32,
    Crc64,
    Sha256,
}

/// Optional `zstd` tuning, parsed from `--zstd-advanced <JSON>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ZstdAdvancedOptions {
    /// Compression level (1–19; overrides `--level`).
    pub level: Option<u32>,
    /// Encoder threads (overrides `--jobs`).
    pub threads: Option<u32>,
    /// Enable long-distance matching with this window log (10–30).
    pub long_window_log: Option<u32>,
    /// Include a checksum in the frame (`true`/`false`).
    pub checksum: Option<bool>,
    /// Store the content size in the frame (`true`/`false`).
    pub content_size: Option<bool>,
    /// Path to a trained dictionary file.
    pub dict: Option<String>,
}

/// Deserialize an optional size (bytes) as either a JSON number, a
/// human-friendly string (`"64MiB"`, `"512M"`, …), or absent.
fn de_opt_size<'de, D>(deserializer: D) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OptSize {
        Num(u64),
        Str(String),
    }
    let s = Option::<OptSize>::deserialize(deserializer)?;
    match s {
        None => Ok(None),
        Some(OptSize::Num(n)) => Ok(Some(n)),
        Some(OptSize::Str(ref str)) => super::parse_memlimit(str)
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}

fn parse_advanced_json<T: for<'de> Deserialize<'de>>(
    format: CompressionFormat, flag: &str, input: &str) -> Result<T> {
    
    let json = if let Some(path) = input.strip_prefix('@') {
        let p = Path::new(path);
        std::fs::read_to_string(p).map_err(|e| Error::io(p, e))?
    } else {
        input.to_string()
    };
    if advanced_help_requested(format, &json)? {
        std::process::exit(0);
    }
    serde_json::from_str(&json).map_err(|e| {
        Error::Config(format!(
            "invalid JSON for {flag} ({}): {e}",
            format.as_str()
        ))
    })
}

/// If the advanced JSON carries a truthy top-level `help` key, print a summary
/// of the accepted keys for `format` to stdout and return `true` (caller exits).
fn advanced_help_requested(format: CompressionFormat, json: &str) -> Result<bool> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(json) {
        if v.get("help").and_then(|h| h.as_bool()).unwrap_or(false) {
            let (title, keys) = match format {
                CompressionFormat::Xz => (
                    "xz advanced options (--xz-advanced JSON keys)",
                    XZ_ADVANCED_HELP,
                ),
                CompressionFormat::Zstd => (
                    "zstd advanced options (--zstd-advanced JSON keys)",
                    ZSTD_ADVANCED_HELP,
                ),
                _ => return Ok(false),
            };
            println!("{title}\n{keys}");
            return Ok(true);
        }
    }
    Ok(false)
}

const XZ_ADVANCED_HELP: &str = "level        LZMA2 preset level 0-9 (overrides --level)\n\
                              extreme      xz -e / --extreme\n\
                              dict         dictionary size, bytes or \"64MiB\" (16 KiB - 1.5 GiB)\n\
                              block_size   MT encoder block size, bytes or \"64MiB\" (0 = liblzma default)\n\
                              threads      encoder threads (overrides --jobs)\n\
                              lc           literal context bits (0-4)\n\
                              lp           literal position bits (0-4)\n\
                              pb           position bits (0-4)\n\
                              mode         fast | normal\n\
                              nice         nice len for the match finder (2-273)\n\
                              mf           hc3 | hc4 | bt2 | bt3 | bt4\n\
                              depth        search depth limit for match finders (0 = preset default)\n\
                              check        none | crc32 | crc64 | sha256\n\
                              memlimit     max encoder RAM, bytes or \"64MiB\"";

const ZSTD_ADVANCED_HELP: &str = "level             compression level (1-19; overrides --level)\n\
                                threads           encoder threads (overrides --jobs)\n\
                                long_window_log   long-distance matching window log (10-30)\n\
                                checksum          include a frame checksum (true/false)\n\
                                content_size      store the content size in the frame (true/false)\n\
                                dict              path to a trained dictionary file";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompressionSettings {
    pub format: CompressionFormat,
    /// Compression level for the active filter (ignored when `format` is None).
    pub level: u32,
    /// xz advanced LZMA2 tuning (from `--xz-advanced`), overlaid on `--level`.
    #[serde(default)]
    pub xz_advanced: XzAdvancedOptions,
    /// zstd advanced tuning (from `--zstd-advanced`), overlaid on `--level`.
    #[serde(default)]
    pub zstd_advanced: ZstdAdvancedOptions,
}

impl Default for CompressionSettings {
    fn default() -> Self {
        Self {
            format: CompressionFormat::None,
            level: 0,
            xz_advanced: XzAdvancedOptions::default(),
            zstd_advanced: ZstdAdvancedOptions::default(),
        }
    }
}

impl CompressionSettings {
    pub fn validate_aux(&self, jobs: usize) -> Result<()> {
        let memlimit = self.xz_advanced.memlimit;
        let threads = self.xz_advanced.threads.map(|t| t as usize).unwrap_or(jobs);
        if self.format == CompressionFormat::Xz {
            let _ = resolve_xz_threads(threads, memlimit, self.xz_level())?;
        }
        Ok(())
    }

    /// Effective xz encoder threads: `--xz-advanced.threads` beats `--jobs`.
    pub fn xz_threads(&self, jobs: usize) -> usize {
        self.xz_advanced.threads.map(|t| t as usize).unwrap_or(jobs)
    }

    /// Effective xz preset: `--level` (plus JSON `level`/`extreme` overrides).
    pub fn xz_preset(&self) -> u32 {
// JSON `level` beats `--level`; the `unwrap_or` alone is the whole
        // resolution (an earlier `preset == 0 && level > 0` re-assignment was
        // dead code — `unwrap_or(self.level)` already yields `self.level` when
        // the JSON `level` is absent).
        let mut preset = self.xz_advanced.level.unwrap_or(self.level);
        if self.xz_advanced.extreme.unwrap_or(false) {
            preset |= 1u32 << 31; // LZMA_PRESET_EXTREME
        }
        preset
    }

    /// Effective xz level (JSON `level` beats `--level`).
    fn xz_level(&self) -> u32 {
        self.xz_advanced.level.unwrap_or(self.level)
    }
}

impl CompressionSettings {
    /// Validate CLI compression options and build settings for archive creation.
    pub fn from_archive_args(format: CompressionFormat, args: &ArchiveArgs) -> Result<Self> {
        let level = Self::resolve_compress_options(format, args)?;
        let xz_advanced = match format {
            CompressionFormat::Xz => match &args.xz_advanced {
                None => XzAdvancedOptions::default(),
                Some(s) => parse_advanced_json::<XzAdvancedOptions>(
                    format, "--xz-advanced", s)?,
            },
            _ => {
                if args.xz_advanced.is_some() {
                    return Err(Error::Config(
                        "--xz-advanced is only valid with xz compression (-J / .tar.xz)".into(),
                    ));
                }
                XzAdvancedOptions::default()
            }
        };
        if let Some(d) = xz_advanced.dict {
            if d > u32::MAX as u64 {
                return Err(Error::Config(format!(
                    "--xz-advanced dict {d} exceeds the LZMA2 maximum ({})",
                    u32::MAX
                )));
            }
        }
        // Bit-count fields are clamped by liblzma (lc 0-4, lp 0-4 and lc+lp<=4,
        // pb 0-4). Catch them up-front so an invalid JSON value becomes a clear
        // config error instead of an opaque encoder failure deep in the archive.
        for (name, val, max) in [
            ("lc", xz_advanced.lc, 4u32),
            ("lp", xz_advanced.lp, 4),
            ("pb", xz_advanced.pb, 4),
        ] {
            if let Some(v) = val {
                if v > max {
                    return Err(Error::Config(format!(
                        "--xz-advanced {name} {v} is out of range (allowed 0-{max})"
                    )));
                }
            }
        }
        if let (Some(lc), Some(lp)) = (xz_advanced.lc, xz_advanced.lp) {
            if lc + lp > 4 {
                return Err(Error::Config(format!(
                    "--xz-advanced lc {lc} + lp {lp} exceeds the LZMA2 combined max (lc+lp <= 4)"
                )));
            }
        }
        // Nice length 2-273 (match length LZMA2 can encode); depth is u32, 0 =
        // liblzma automatic default, no upper bound worth policing.
        if let Some(v) = xz_advanced.nice {
            if v < 2 || v > 273 {
                return Err(Error::Config(format!(
                    "--xz-advanced nice {v} is out of range (allowed 2-273)"
                )));
            }
        }
        if let Some(l) = xz_advanced.level {
            if l > 9 {
                return Err(Error::Config(format!(
                    "--xz-advanced level {l} is out of range (allowed 0–9)"
                )));
            }
        }
        let zstd_advanced = match format {
            CompressionFormat::Zstd => match &args.zstd_advanced {
                None => ZstdAdvancedOptions::default(),
                Some(s) => parse_advanced_json::<ZstdAdvancedOptions>(
                    format, "--zstd-advanced", s)?,
            },
            _ => {
                if args.zstd_advanced.is_some() {
                    return Err(Error::Config(
                        "--zstd-advanced is only valid with zstd compression (--zstd / .tar.zst)".into(),
                    ));
                }
                ZstdAdvancedOptions::default()
            }
        };
        if let Some(l) = zstd_advanced.level {
            if l < 1 || l > 19 {
                return Err(Error::Config(format!(
                    "--zstd-advanced level {l} is out of range (allowed 1–19)"
                )));
            }
        }
        Ok(Self {
            format,
            level,
            xz_advanced,
            zstd_advanced,
        })
    }

    fn resolve_compress_options(format: CompressionFormat, args: &ArchiveArgs) -> Result<u32> {
        if format == CompressionFormat::None {
            if args.level.is_some() {
                return Err(Error::Config(
                    "--level requires a compression filter (archive is uncompressed)".into(),
                ));
            }
            return Ok(0);
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
        Ok(level)
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

        // --xz-extreme / --xz-memlimit-compress on a non-xz filter.
        for extra in [vec!["--xz-extreme"], vec!["--xz-memlimit-compress", "64MiB"]] {
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
        let args = archive_args("a.tar", &["--xz-memlimit-compress", "64MiB"]);
        let s = CompressionSettings::from_archive_args(CompressionFormat::Xz, &args)
            .expect("xz memlimit accepted");
        assert_eq!(s.memlimit_compress, Some(64 * 1024 * 1024));
    }
}
