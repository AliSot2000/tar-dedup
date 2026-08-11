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
    /// Max RAM for xz MT encoder (`None` = no limit, like default `xz`).
    pub memlimit_compress: Option<u64>,
}

impl CompressionSettings {
    /// Validate CLI compression options and build settings for archive creation.
    pub fn from_archive_args(format: CompressionFormat, args: &ArchiveArgs) -> Result<Self> {
        let (level, xz_extreme) = resolve_compress_options(format, args)?;
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
            memlimit_compress,
        })
    }

    /// Defaults for extract: format-default level, no extreme/small/memlimit.
    pub fn for_extract(format: CompressionFormat) -> Self {
        Self {
            format,
            level: format.default_level().unwrap_or(0),
            xz_extreme: false,
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

    /// Path-resolution base from archive `-C` / `--directory` (absolute), if set.
    pub directory: Option<PathBuf>,

    // TODO: Convert to abs path immediately.
    /// Archive input root (`archive` subcommand only). First of `input_dirs` when non-empty.
    pub input_dir: PathBuf,

    /// All archive input roots from repeated `-i` / `--input-dir`.
    pub input_dirs: Vec<PathBuf>,

    /// Paths from repeated `-T` / `--files-from` (may include `-` for stdin).
    pub files_from: Vec<PathBuf>,

    /// When true, `-T` lists use NUL separators; otherwise newlines.
    pub files_from_null: bool,

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
    /// Canonical election always skips errored files.
    pub dedup_fail_fast: bool,

    /// Broader fail-fast from CLI (`--fail-fast`); currently aliased onto `dedup_fail_fast` too.
    pub fail_fast: bool,

    /// Persist per-file error `Display` strings and continue (phase wiring later).
    pub no_errors: bool,

    /// Sparse/hash zero-page size in bytes.
    pub page_size: usize,
    /// Optional minimum empty-page count before a file is worth sparsifying (used by sparsify).
    pub min_pages: Option<u64>,

    /// When true, run the sparsify phase (phase wiring later).
    pub sparsify: bool,

    /// Exclude regex patterns from `--exclude`.
    pub exclude_patterns: Vec<String>,
    /// Include regex patterns from `--include`.
    pub include_patterns: Vec<String>,
    /// Paths to exclude-pattern files from `-X` / `--exclude-from`.
    pub exclude_from: Vec<PathBuf>,
    /// Paths to include-pattern files from `--include-from`.
    pub include_from: Vec<PathBuf>,

    pub no_recursion: bool,
    pub dereference: bool,
    pub one_file_system: bool,
    pub absolute_names: bool,
    pub no_hardlink_detection: bool,
    pub anchored: bool,
    pub ignore_case: bool,

    /// Force owner policy: `NAME`, `UID`, or `NAME:UID` (archive meta / extract apply).
    pub owner: Option<String>,
    /// Path to GNU-style owner map file.
    pub owner_map: Option<PathBuf>,
    /// Force group policy: `NAME`, `GID`, or `NAME:GID`.
    pub group: Option<String>,
    /// Path to GNU-style group map file.
    pub group_map: Option<PathBuf>,

    pub eager_filter: bool,
    pub no_dedup: bool,

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

    /// After extracting, recompute the sha1 of each file to ensure it is correct.
    pub rehash: bool,
}   

impl Config {
    pub fn from_archive_args(args: &ArchiveArgs) -> Result<Self> {
        if args.exclude_vcs || args.exclude_vcs_ignores {
            return Err(Error::Config(
                "--exclude-vcs / --exclude-vcs-ignores are not implemented yet".into(),
            ));
        }

        if args.input_dirs.is_empty() && args.files_from.is_empty() {
            return Err(Error::Config(
                "at least one of `-i`/`--input-dir` or `-T`/`--files-from` is required".into(),
            ));
        }

        if args.page_size == 0 {
            return Err(Error::Config("page_size must be greater than 0".into()));
        }

        let base = resolution_base(args.directory.as_deref())?;

        let archive_path = resolve_user_path_against(&args.archive, &base)?;
        if let Some(parent) = archive_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        let mut input_dirs = Vec::with_capacity(args.input_dirs.len());
        for dir in &args.input_dirs {
            let resolved = resolve_user_path_against(dir, &base)?;
            validate_dir(&resolved, "input directory")?;
            input_dirs.push(resolved);
        }
        let input_dir = input_dirs.first().cloned().unwrap_or_default();

        let files_from: Vec<PathBuf> = args
            .files_from
            .iter()
            .map(|p| {
                if p.as_os_str() == "-" {
                    Ok(PathBuf::from("-"))
                } else {
                    resolve_user_path_against(p, &base)
                }
            })
            .collect::<Result<_>>()?;

        let exclude_from: Vec<PathBuf> = args
            .exclude_from
            .iter()
            .map(|p| resolve_user_path_against(p, &base))
            .collect::<Result<_>>()?;
        let include_from: Vec<PathBuf> = args
            .include_from
            .iter()
            .map(|p| resolve_user_path_against(p, &base))
            .collect::<Result<_>>()?;

        let owner_map = args
            .owner_map
            .as_ref()
            .map(|p| resolve_user_path_against(p, &base))
            .transpose()?;
        let group_map = args
            .group_map
            .as_ref()
            .map(|p| resolve_user_path_against(p, &base))
            .transpose()?;

        let work_dir = match &args.work_dir {
            Some(path) => resolve_user_path_against(path, &base)?,
            None => default_archive_work_dir(&archive_path),
        };
        std::fs::create_dir_all(&work_dir).map_err(|e| Error::io(&work_dir, e))?;

        let format = resolve_compression(&args.compression, &archive_path)?;
        let compression = CompressionSettings::from_archive_args(format, args)?;
        // Archive CLI no longer exposes `--resume` (future subcommand); only `--fresh`.
        let start_policy = crate::common::start::StartPolicy::from_flags(false, args.fresh)?;
        let jobs = args.jobs.unwrap_or_else(num_cpus::get);

        let directory = args
            .directory
            .as_ref()
            .map(|_| base.clone());

        Ok(Self {
            archive_path,
            directory,
            input_dir,
            input_dirs,
            files_from,
            files_from_null: args.null,
            output_dir: PathBuf::new(),
            work_dir,
            compression,
            jobs,
            start_policy,
            cleanup: CleanupSettings::from_flags(args.keep_db, args.keep_stage),
            extract_stage_location: ExtractStageLocation::BesideArchive,
            exit_after_stage: args.exit_after_stage.map(ExitAfterStage::from),
            restore_owner: false,
            do_xattrs: args.xattrs,
            do_posix_acl: args.acls,
            do_selinux: args.selinux,
            dedup_fail_fast: args.fail_fast,
            fail_fast: args.fail_fast,
            no_errors: args.no_errors,
            page_size: args.page_size,
            min_pages: args.min_pages,
            sparsify: args.sparsify,
            exclude_patterns: args.exclude.clone(),
            include_patterns: args.include.clone(),
            exclude_from,
            include_from,
            no_recursion: args.no_recursion,
            dereference: args.dereference,
            one_file_system: args.one_file_system,
            absolute_names: args.absolute_names,
            no_hardlink_detection: args.no_hardlink_detection,
            anchored: args.anchored,
            ignore_case: args.ignore_case,
            owner: args.owner.clone(),
            owner_map,
            group: args.group.clone(),
            group_map,
            eager_filter: args.eager_filter,
            no_dedup: args.no_dedup,
            write_archive_footer: true,
            retry_missing_sha: args.retry_missing_sha,
            force_scan: false, // TODO CLI Arg
            clear_archive_meta: false, // TODO CLI Arg
            rehash: true, // TODO CLI Arg
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
            directory: None,
            input_dir: PathBuf::new(),
            input_dirs: Vec::new(),
            files_from: Vec::new(),
            files_from_null: false,
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
            fail_fast: false,
            no_errors: false,
            page_size: 4096,
            min_pages: Some(0),
            sparsify: false,
            exclude_patterns: Vec::new(),
            include_patterns: Vec::new(),
            exclude_from: Vec::new(),
            include_from: Vec::new(),
            no_recursion: false,
            dereference: false,
            one_file_system: false,
            absolute_names: false,
            no_hardlink_detection: false,
            anchored: false,
            ignore_case: false,
            owner: None,
            owner_map: None,
            group: None,
            group_map: None,
            eager_filter: false,
            no_dedup: false,
            write_archive_footer: true,
            retry_missing_sha: false,
            force_scan: false, // TODO Add CLI Arg
            clear_archive_meta: false, // TODO CLI Arg
            rehash: true, // TODO CLI Arg
        })
    }

    pub fn db_path(&self) -> PathBuf {
        self.work_dir.join("tar-dedup.sqlite")
    }

    pub fn temp_db(&self) -> PathBuf { self.work_dir.join("temp-tar-dedup.sqlite")}

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

    if flags.auto_compress && flags.no_auto_compress {
        return Err(Error::Config(
            "--no-auto-compress and --auto-compress cannot be passed at the same time.".to_string()
        ));
    }

    Ok(CompressionFormat::None)
}

/// Validate `--level` / `--xz-extreme` / `--bzip-small` against the resolved filter.
fn resolve_compress_options(
    format: CompressionFormat,
    args: &ArchiveArgs,
) -> Result<(u32, bool)> {
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
    let mut level = args.level.unwrap_or_else(|| format.default_level().unwrap());
    if level < min || level > max {
        return Err(Error::Config(format!(
            "--level {level} is out of range for {} (allowed {min}–{max})",
            format.as_str()
        )));
    }


    Ok((level, args.xz_extreme))
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
    let cwd = std::env::current_dir().map_err(Error::from)?;
    resolve_user_path_against(path, &cwd)
}

/// Resolve `path` against `base` (absolute paths unchanged).
pub fn resolve_user_path_against(path: &Path, base: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(base.join(path))
    }
}

fn resolution_base(directory: Option<&Path>) -> Result<PathBuf> {
    match directory {
        None => std::env::current_dir().map_err(Error::from),
        Some(dir) => {
            let resolved = resolve_user_path(dir)?;
            validate_dir(&resolved, "directory")?;
            Ok(resolved)
        }
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
        // `Path::parent` of a bare root yields `None`, of `"foo"` yields `Some("")`.
        _ if path.has_root() => Path::new("/"),
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
    Inventory,
    Hash,
    Filter,
    Dedup,
    Sparsify,
    Stage,
    Archive,
    Cleanup,
}

impl From<ExitAfterStageArg> for ExitAfterStage {
    fn from(arg: ExitAfterStageArg) -> Self {
        match arg {
            ExitAfterStageArg::Inventory => Self::Inventory,
            ExitAfterStageArg::Hash => Self::Hash,
            ExitAfterStageArg::Filter => Self::Filter,
            ExitAfterStageArg::Dedup => Self::Dedup,
            ExitAfterStageArg::Sparsify => Self::Sparsify,
            ExitAfterStageArg::Stage => Self::Stage,
            ExitAfterStageArg::Archive => Self::Archive,
            ExitAfterStageArg::Cleanup => Self::Cleanup,
        }
    }
}

impl ExitAfterStage {
    /// Pipeline phase whose successful completion triggers exit (`None` = run through cleanup).
    pub fn stop_after_phase(self) -> Option<PipelinePhase> {
        match self {
            Self::Inventory => Some(PipelinePhase::Inventory),
            Self::Hash => Some(PipelinePhase::Hash),
            Self::Filter => Some(PipelinePhase::Filter),
            Self::Dedup => Some(PipelinePhase::Dedup),
            Self::Sparsify => Some(PipelinePhase::Sparsify),
            Self::Stage => Some(PipelinePhase::Stage),
            Self::Archive => Some(PipelinePhase::Archive),
            Self::Cleanup => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PipelinePhase {
    Inventory,
    Hash,
    Filter,
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
            Self::Filter => "filter",
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
            Self::Hash => Some(Self::Filter),
            Self::Filter => Some(Self::Dedup),
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
            "filter" => Ok(Self::Filter),
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

/// Extract pipeline driver phase (persisted in meta as `extract_phase`).
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
