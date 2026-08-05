use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cli::{ArchiveArgs, CompressionFlags, ExitAfterStageArg, ExtractArgs};
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
            Self::Xz => true,
            Self::Gz => true,
            Self::Bz2 => true,
            Self::Zstd => true,
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
    /// bzip2 `-s` (small blocks ≈ level 1).
    pub bzip_small: bool,
    /// Max RAM for xz MT encoder (`None` = no limit, like default `xz`).
    pub memlimit_compress: Option<u64>,
}

impl CompressionSettings {
    /// Validate CLI compression options and build settings for archive creation.
    pub fn from_archive_args(format: CompressionFormat, args: &ArchiveArgs) -> Result<Self> {
        let (level, xz_extreme, bzip_small) = resolve_compress_options(format, args)?;
        let memlimit_compress = args
            .memlimit_compress
            .as_deref()
            .map(parse_memlimit)
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
            bzip_small,
            memlimit_compress,
        })
    }

    /// Defaults for extract: format-default level, no extreme/small/memlimit.
    pub fn for_extract(format: CompressionFormat) -> Self {
        Self {
            format,
            level: format.default_level().unwrap_or(0),
            xz_extreme: false,
            bzip_small: false,
            memlimit_compress: None,
        }
    }
}

/// Where extract places `{stem}.estage` (config-only; not wired to CLI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExtractStageLocation {
    /// `{archive_path.parent}/{stem}.estage`
    #[default]
    BesideArchive,
    /// `{output_dir.parent()}/{stem}.estage`
    BesideOutput,
}

/// Post-success keep flags for the work directory / retained DB.
#[derive(Debug, Clone, Copy, Default)]
pub struct CleanupSettings {
    pub keep_db: bool,
    pub keep_stage: bool,
}

impl CleanupSettings {
    pub fn from_flags(keep_db: bool, keep_stage: bool) -> Self {
        Self { keep_db, keep_stage }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Path to archive being created or to archive being extracted filename.tar[.compression]
    pub archive_path: PathBuf,

    // TODO: Convert to abs path immediately.
    /// Archive input root (`archive` subcommand only).
    pub input_dir: PathBuf,

    /// Extract output root (`extract` subcommand `-C`).
    pub output_dir: PathBuf,

    /// `.astage` / `.estage` work directory (DB + payloads live directly here).
    pub work_dir: PathBuf,
    pub compression: CompressionSettings,
    pub jobs: usize,
    pub start_policy: crate::common::start::StartPolicy,
    pub cleanup: CleanupSettings,
    /// Extract only: parent directory for `.estage` (default beside archive).
    pub extract_stage_location: ExtractStageLocation,
    pub exit_after_stage: Option<ExitAfterStage>,
    /// Extract: restore uid/gid when possible.
    pub restore_owner: bool,

    /// Capture xattrs
    pub do_xattrs: bool,
    /// Capture posix_acls
    pub do_posix_acl: bool,
    /// Capture SELinux context
    pub do_selinux: bool,

    /// When true, skip `ErrorWhileDedup` files as compare candidates each round.
    /// Canonical election always skips errored files. No CLI wiring yet.
    pub dedup_fail_fast: bool,

    /// Sparse/hash zero-page size in bytes.
    pub page_size: usize,
    /// Optional minimum empty-page count before a file is worth sparsifying (used by sparsify).
    pub min_pages: Option<u64>,

    /// When true, append seekable sqlite footer after a finished archive stream.
    /// Internal/test knob — not wired to CLI.
    pub write_archive_footer: bool,

    /// When true, stage attempts to link files that failed to be hashed into stage and archive
    /// pass will subsequently attempt to add those files to the archive.
    pub retry_missing_sha: bool,

    /// Force the scan of a tar archive that does contain content-ids but that does not start with
    /// a manifest. Errors will still be produced, if a file does not match the nomenclature
    pub force_scan: bool,
    
    /// When true removes the metadata that was added to the database to execute the archival 
    /// process. This will be done at the very end of the archive process or at the very beginning 
    /// of the extraction process.
    pub clear_archive_meta: bool,
}   

impl Config {
    pub fn from_archive_args(args: &ArchiveArgs) -> Result<Self> {
        let input_dir = resolve_user_path(&args.input)?;
        validate_dir(&input_dir, "input directory")?;

        let archive_path = resolve_user_path(&args.archive)?;
        if let Some(parent) = archive_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        let work_dir = match &args.work_dir {
            Some(path) => resolve_user_path(path)?,
            None => default_archive_work_dir(&archive_path),
        };
        std::fs::create_dir_all(&work_dir).map_err(|e| Error::io(&work_dir, e))?;

        let format = resolve_compression(&args.compression, &archive_path)?;
        let compression = CompressionSettings::from_archive_args(format, args)?;
        let start_policy = crate::common::start::StartPolicy::from_flags(args.resume, args.fresh)?;
        let jobs = args.jobs.unwrap_or_else(num_cpus::get);

        Ok(Self {
            archive_path,
            input_dir,
            output_dir: PathBuf::new(),
            work_dir,
            compression,
            jobs,
            start_policy,
            cleanup: CleanupSettings::from_flags(args.keep_db, args.keep_stage),
            extract_stage_location: ExtractStageLocation::BesideArchive,
            exit_after_stage: args.exit_after_stage.map(ExitAfterStage::from),
            restore_owner: false,
            do_xattrs: true,
            do_posix_acl: true,
            do_selinux: true,
            dedup_fail_fast: false,
            page_size: args.page_size,
            min_pages: args.min_pages,
            write_archive_footer: true,
            retry_missing_sha: false,
            force_scan: false, // TODO CLI ARg
            clear_archive_meta: false // TODO CLI Arg
        })
    }

    pub fn from_extract_args(args: &ExtractArgs) -> Result<Self> {
        let archive_path = resolve_user_path(&args.archive)?;
        if !archive_path.is_file() {
            return Err(Error::Config(format!(
                "archive does not exist or is not a file: {}",
                archive_path.display()
            )));
        }

        let output_dir = resolve_user_path(&args.output_dir)?;
        std::fs::create_dir_all(&output_dir).map_err(|e| Error::io(&output_dir, e))?;

        let extract_stage_location = ExtractStageLocation::BesideArchive;
        let work_dir = default_extract_work_dir(&archive_path, &output_dir, extract_stage_location);
        std::fs::create_dir_all(&work_dir).map_err(|e| Error::io(&work_dir, e))?;

        let format = infer_compression_from_suffix(&archive_path);
        let start_policy = crate::common::start::StartPolicy::from_flags(args.resume, args.fresh)?;

        Ok(Self {
            archive_path,
            input_dir: PathBuf::new(),
            output_dir,
            work_dir,
            compression: CompressionSettings::for_extract(format),
            jobs: 1,
            start_policy,
            cleanup: CleanupSettings::from_flags(args.keep_db, args.keep_stage),
            extract_stage_location,
            exit_after_stage: None,
            restore_owner: args.restore_owner,
            do_xattrs: true,
            do_posix_acl: true,
            do_selinux: true,
            dedup_fail_fast: false,
            page_size: 4096,
            min_pages: Some(0),
            write_archive_footer: true,
            retry_missing_sha: false,
            force_scan: false, // TODO Add CLI Arg
            clear_archive_meta: false // TODO CLI Arg
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.work_dir.join("tar-dedup.sqlite")
    }

    /// Archive payload directory — same as `work_dir` (flat `.astage`).
    pub fn stage_dir(&self) -> PathBuf {
        self.work_dir.clone()
    }

    /// Extract payload directory — same as `work_dir` (flat `.estage`).
    pub fn extract_cache_dir(&self) -> PathBuf {
        self.work_dir.clone()
    }
}

pub fn resolve_compression(flags: &CompressionFlags, archive_path: &Path) -> Result<CompressionFormat> {
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

    Ok(CompressionFormat::None)
}

/// Validate `--level` / `--xz-extreme` / `--bzip-small` against the resolved filter.
fn resolve_compress_options(
    format: CompressionFormat,
    args: &ArchiveArgs,
) -> Result<(u32, bool, bool)> {
    if args.xz_extreme && format != CompressionFormat::Xz {
        return Err(Error::Config(
            "--xz-extreme is only valid with xz compression (-J / .tar.xz)".into(),
        ));
    }
    if args.bzip_small && format != CompressionFormat::Bz2 {
        return Err(Error::Config(
            "--bzip-small is only valid with bzip2 compression (-j / .tar.bz2)".into(),
        ));
    }
    if format == CompressionFormat::None {
        if args.level.is_some() {
            return Err(Error::Config(
                "--level requires a compression filter (archive is uncompressed)".into(),
            ));
        }
        return Ok((0, false, false));
    }

    let (min, max) = format
        .level_range()
        .expect("compressing format has a level range");
    let mut level = args.level.unwrap_or_else(|| format.default_level().unwrap());
    if level < min || level > max {
        return Err(Error::Config(format!(
            "--level {level} is out of range for {} (allowed {min}–{max})",
            format.as_str()
        )));
    }

    if args.bzip_small {
        // bzip2 CLI `-s` uses 100k blocks (level 1 memory profile).
        if args.level.is_some() && level != 1 {
            return Err(Error::Config(
                "--bzip-small implies 100k blocks (level 1); omit --level or use --level 1".into(),
            ));
        }
        level = 1;
    }

    Ok((level, args.xz_extreme, args.bzip_small))
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
        // TODO different resolution
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

/// Archive basename with compound compression / `.tar` suffixes stripped.
pub fn archive_stem(archive_path: &Path) -> String {
    let name = archive_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive".into());
    let lower = name.to_ascii_lowercase();
    let strip_len = if lower.ends_with(".tar.xz") {
        ".tar.xz".len()
    } else if lower.ends_with(".tar.gz") {
        ".tar.gz".len()
    } else if lower.ends_with(".tar.bz2") {
        ".tar.bz2".len()
    } else if lower.ends_with(".tar.zst") {
        ".tar.zst".len()
    } else if lower.ends_with(".txz") {
        ".txz".len()
    } else if lower.ends_with(".tgz") {
        ".tgz".len()
    } else if lower.ends_with(".tbz2") {
        ".tbz2".len()
    } else if lower.ends_with(".tbz") {
        ".tbz".len()
    } else if lower.ends_with(".tzst") {
        ".tzst".len()
    } else if lower.ends_with(".tar") {
        ".tar".len()
    } else {
        // Fall back to file_stem behavior for unknown suffixes.
        return archive_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or(name);
    };
    name[..name.len() - strip_len].to_string()
}

/// Parent directory for placing siblings of `path`.
///
/// - `/data/foo.tar.gz` → `/data`
/// - `/foo.tar.gz` → `/`
/// - `/` → `/`
/// - `foo.tar.gz` (relative) → `.`
pub(crate) fn path_parent(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        // `Path::parent` of `/` (and similar) yields `Some("")`; keep the root.
        Some(_) if path.has_root() => Path::new("/"),
        _ => Path::new("."),
    }
}

fn default_extract_work_dir(
    archive_path: &Path,
    output_dir: &Path,
    location: ExtractStageLocation,
) -> PathBuf {
    let parent = match location {
        ExtractStageLocation::BesideArchive => path_parent(archive_path),
        ExtractStageLocation::BesideOutput => path_parent(output_dir),
    };
    parent.join(format!("{}.estage", archive_stem(archive_path)))
}

fn default_archive_work_dir(archive_path: &Path) -> PathBuf {
    path_parent(archive_path).join(format!("{}.astage", archive_stem(archive_path)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn archive_stem_strips_compound_suffixes() {
        assert_eq!(archive_stem(Path::new("backup.tar.gz")), "backup");
        assert_eq!(archive_stem(Path::new("/tmp/x.tar.xz")), "x");
        assert_eq!(archive_stem(Path::new("a.tgz")), "a");
        assert_eq!(archive_stem(Path::new("n.tar")), "n");
    }

    #[test]
    fn default_dirs_use_astage_estage() {
        let arch = Path::new("/data/foo.tar.gz");
        assert_eq!(default_archive_work_dir(arch), PathBuf::from("/data/foo.astage"));
        assert_eq!(
            default_extract_work_dir(arch, Path::new("/out"), ExtractStageLocation::BesideArchive),
            PathBuf::from("/data/foo.estage")
        );
        assert_eq!(
            default_extract_work_dir(arch, Path::new("/out/tree"), ExtractStageLocation::BesideOutput),
            PathBuf::from("/out/foo.estage")
        );
    }

    #[test]
    fn path_parent_keeps_filesystem_root() {
        assert_eq!(path_parent(Path::new("/foo.tar.gz")), Path::new("/"));
        assert_eq!(path_parent(Path::new("/")), Path::new("/"));
        assert_eq!(path_parent(Path::new("foo.tar.gz")), Path::new("."));
        assert_eq!(path_parent(Path::new("/data/foo.tar.gz")), Path::new("/data"));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitAfterStage {
    Scan,
    Hash,
    Dedup,
    Sparsify,
    Stage,
    Tar,
    Cleanup,
}

impl From<ExitAfterStageArg> for ExitAfterStage {
    fn from(arg: ExitAfterStageArg) -> Self {
        match arg {
            ExitAfterStageArg::Scan => Self::Scan,
            ExitAfterStageArg::Hash => Self::Hash,
            ExitAfterStageArg::Dedup => Self::Dedup,
            ExitAfterStageArg::Sparsify => Self::Sparsify,
            ExitAfterStageArg::Stage => Self::Stage,
            ExitAfterStageArg::Tar => Self::Tar,
            ExitAfterStageArg::Cleanup => Self::Cleanup,
        }
    }
}

impl ExitAfterStage {
    /// Pipeline phase whose successful completion triggers exit (`None` = run through cleanup).
    pub fn stop_after_phase(self) -> Option<PipelinePhase> {
        match self {
            Self::Scan => Some(PipelinePhase::Inventory),
            Self::Hash => Some(PipelinePhase::Hash),
            Self::Dedup => Some(PipelinePhase::Dedup),
            Self::Sparsify => Some(PipelinePhase::Sparsify),
            Self::Stage => Some(PipelinePhase::Stage),
            Self::Tar => Some(PipelinePhase::Archive),
            Self::Cleanup => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipelinePhase {
    Inventory,
    Hash,
    Dedup,
    Sparsify,
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
            Self::Sparsify => "sparsify",
            Self::Stage => "stage",
            Self::Archive => "archive",
            Self::Done => "done",
        }
    }

    pub fn next(self) -> Option<Self> {
        match self {
            Self::Inventory => Some(Self::Hash),
            Self::Hash => Some(Self::Dedup),
            Self::Dedup => Some(Self::Sparsify),
            Self::Sparsify => Some(Self::Stage),
            Self::Stage => Some(Self::Archive),
            Self::Archive => Some(Self::Done),
            Self::Done => None,
        }
    }

    pub fn parse(raw: &str) -> crate::error::Result<Self> {
        match raw {
            "inventory" => Ok(Self::Inventory),
            "hash" => Ok(Self::Hash),
            "dedup" => Ok(Self::Dedup),
            "sparsify" => Ok(Self::Sparsify),
            "stage" => Ok(Self::Stage),
            "archive" => Ok(Self::Archive),
            "done" => Ok(Self::Done),
            other => Err(crate::error::Error::Config(format!(
                "unknown pipeline phase: {other}"
            ))),
        }
    }
}

/// Extract pipeline driver phase (persisted in meta `extract_phase`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtractPipelinePhase {
    /// Scan tar stream into cache; ingest snapshot.sqlite copies into the extract work DB.
    ScanTar,
    /// Recompute hashes to detect corruption (stub).
    Rehash,
    /// Place extracted payloads / links at final rel_paths.
    Place,
    /// Apply mode/owner/xattrs/ACLs/SELinux (stub).
    Permissions,
    /// Remove temporary extract files and embedded snapshot copies.
    Cleanup,
    Done,
}

impl ExtractPipelinePhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScanTar => "scan_tar",
            Self::Rehash => "rehash",
            Self::Place => "place",
            Self::Permissions => "permissions",
            Self::Cleanup => "cleanup",
            Self::Done => "done",
        }
    }

    pub fn parse(raw: &str) -> crate::error::Result<Self> {
        match raw {
            "scan_tar" => Ok(Self::ScanTar),
            "rehash" => Ok(Self::Rehash),
            "place" => Ok(Self::Place),
            "permissions" => Ok(Self::Permissions),
            "cleanup" => Ok(Self::Cleanup),
            "done" => Ok(Self::Done),
            other => Err(crate::error::Error::Config(format!(
                "unknown extract pipeline phase: {other}"
            ))),
        }
    }

    pub fn next(self) -> Option<Self> {
        match self {
            Self::ScanTar => Some(Self::Rehash),
            Self::Rehash => Some(Self::Place),
            Self::Place => Some(Self::Permissions),
            Self::Permissions => Some(Self::Cleanup),
            Self::Cleanup => Some(Self::Done),
            Self::Done => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractRuntimeState {
    pub phase: ExtractPipelinePhase,
    /// Number of snapshot.sqlite members ingested from the archive so far.
    pub snapshots_ingested: u32,
}

impl ExtractRuntimeState {
    pub fn new() -> Self {
        Self {
            phase: ExtractPipelinePhase::ScanTar,
            snapshots_ingested: 0,
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

/// Parse `xz`-style memory limits: raw bytes, `MiB`/`GiB`, or `%` of physical RAM.
fn parse_memlimit(s: &str) -> Result<u64> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        let pct: u64 = pct
            .trim()
            .parse()
            .map_err(|_| Error::Config(format!("invalid memlimit percentage: {s}")))?;
        if pct == 0 || pct > 100 {
            return Err(Error::Config(format!(
                "memlimit percentage must be 1–100, got {pct}%"
            )));
        }
        let ram = physical_ram_bytes().ok_or_else(|| {
            Error::Config("cannot read physical RAM for memlimit percentage".into())
        })?;
        return Ok(pct * ram / 100);
    }

    let (num, scale) = if let Some(v) = s.strip_suffix("GiB") {
        (v.trim(), 1024u64 * 1024 * 1024)
    } else if let Some(v) = s.strip_suffix("G") {
        (v.trim(), 1000u64 * 1000 * 1000)
    } else if let Some(v) = s.strip_suffix("MiB") {
        (v.trim(), 1024u64 * 1024)
    } else if let Some(v) = s.strip_suffix("M") {
        (v.trim(), 1000u64 * 1000)
    } else if let Some(v) = s.strip_suffix("KiB") {
        (v.trim(), 1024u64)
    } else if let Some(v) = s.strip_suffix("K") {
        (v.trim(), 1000u64)
    } else {
        (s, 1u64)
    };

    let n: u64 = num
        .parse()
        .map_err(|_| Error::Config(format!("invalid memlimit: {s}")))?;
    n.checked_mul(scale)
        .ok_or_else(|| Error::Config(format!("memlimit overflow: {s}")))
}

fn physical_ram_bytes() -> Option<u64> {
    let line = std::fs::read_to_string("/proc/meminfo")
        .ok()?
        .lines()
        .find(|l| l.starts_with("MemTotal:"))?
        .to_string();
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}
