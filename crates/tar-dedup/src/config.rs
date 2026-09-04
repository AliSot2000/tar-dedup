mod archive;
mod compression;
mod extract;
mod paths;
mod phases;
mod process;
mod resume;

pub use archive::{
    ArchiveConfig, ArchivePipelineOptions, CaptureOptions, FilterOptions, IndexingOptions,
    InputOptions, OwnerPolicy, SparseOptions,
};
pub use compression::{
    infer_compression_from_suffix, resolve_compression, CompressionFormat, CompressionSettings,
};
pub use extract::{
    ExtractAttributeOptions, ExtractConfig, PlacementOptions, ScanOptions,
};
pub use paths::{PathLayout, PathSource};
pub use phases::{
    ExtractPipelinePhase, ExtractRuntimeState, PipelinePhase, RuntimeState,
};
pub use process::{
    CleanupSettings, ExitAfterStage, ProcessOptions, ResumeOverrides,
};
pub use resume::ResumeConfig;

use std::path::{Path, PathBuf};

use path_clean::PathClean;

use crate::error::{Error, Result};

/// Top-level dispatch wrapper after CLI parse.
#[derive(Debug)]
pub enum RunConfig {
    Archive(ArchiveConfig),
    Extract(ExtractConfig),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupMode {
    Archive,
    Extract,
}

/// Shared work-directory layout for cleanup helpers.
pub trait WorkLayout {
    fn paths(&self) -> &PathLayout;
    fn cleanup(&self) -> &CleanupSettings;
    fn kept_db_parent(&self, mode: CleanupMode) -> &Path;
}

/// Where extract places `{stem}.estage` (builder-only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExtractStageLocation {
    #[default]
    BesideArchive,
    BesideOutput,
}

pub fn resolve_user_path(path: &Path) -> Result<PathBuf> {
    let cwd = std::env::current_dir().map_err(Error::from)?;
    Ok(resolve_path_to_abs_path(path, &cwd))
}

/// Resolves a path to an absolute path by assembling an abspath from the given base.
pub fn resolve_path_to_abs_path(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf().clean()
    } else {
        base.join(path).clean()
    }
}

pub(crate) fn resolve_cwd(directory: Option<&Path>) -> Result<PathBuf> {
    let resolved = match directory {
        None => std::env::current_dir().map_err(Error::from)?.clean(),
        Some(dir) => {
            if dir.is_absolute() {
                dir.to_path_buf().clean()
            } else {
                std::env::current_dir()
                    .map_err(Error::from)?
                    .join(dir)
                    .clean()
            }
        }
    };
    validate_dir(&resolved, "--directory")?;
    Ok(resolved)
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
        return archive_path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or(name);
    };
    name[..name.len() - strip_len].to_string()
}

pub(crate) fn path_parent(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ if path.has_root() => Path::new("/"),
        _ => Path::new("."),
    }
}

pub(crate) fn default_extract_work_dir(
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

pub(crate) fn default_archive_work_dir(archive_path: &Path) -> PathBuf {
    path_parent(archive_path).join(format!("{}.astage", archive_stem(archive_path)))
}

pub(crate) fn validate_dir(path: &Path, label: &str) -> Result<()> {
    if !path.is_dir() {
        return Err(Error::Config(format!(
            "{label} does not exist or is not a directory: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn validate_file(path: &Path, label: &str) -> Result<()> {
    if !path.is_file() {
        return Err(Error::Config(format!(
            "{label} does not exist or is not a file: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn parse_memlimit(s: &str) -> Result<u64> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            default_archive_work_dir(arch),
            PathBuf::from("/data/foo.astage")
        );
        assert_eq!(
            default_extract_work_dir(arch, Path::new("/out"), ExtractStageLocation::BesideArchive),
            PathBuf::from("/data/foo.estage")
        );
        assert_eq!(
            default_extract_work_dir(
                arch,
                Path::new("/out/tree"),
                ExtractStageLocation::BesideOutput
            ),
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