mod archive;
mod compression;
mod extract;
mod paths;
pub mod phases;
mod process;
mod resume;
mod common;
mod filter;

pub use archive::{
    ArchiveConfig, ArchivePipelineOptions, CaptureOptions, IndexingOptions,
    InputOptions, OwnerPolicy, SparseOptions,
};
pub use filter::FilterOptions;
pub use compression::{
    CompressionFormat, CompressionSettings, infer_compression_from_suffix, resolve_compression,
};
pub use extract::{
    ExtractAttributeOptions, ExtractConfig, OwnerGroupOptions, PlacementOptions, ScanOptions,
};
pub use paths::{PathLayout, PathSource};
pub use phases::{ExtractPipelinePhase, ExtractRuntimeState, PipelinePhase, ArchiveRuntimeState};
pub use process::{CleanupSettings, ExitAfterStage, ProcessOptions, ResumeOverrides};
pub use resume::ResumeConfig;

use std::path::{Path, PathBuf};

use path_clean::PathClean;

use crate::error::{Error, Result};

/// Cross-field validation on a completed config block.
///
/// Container configs validate their child blocks first (recursively), then their
/// own invariants. `build` runs this on the **merged** result — a field may be
/// valid in isolation but invalid once combined with an inherited base value
/// (e.g. `clean_target` + `no_create_dir` from two different sources).
pub trait Validate {
    fn validate(&self) -> Result<()>;
}

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum WorkDirLocation {
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
    location: WorkDirLocation)
    -> PathBuf {
    let parent = match location {
        WorkDirLocation::BesideArchive => path_parent(archive_path),
        WorkDirLocation::BesideOutput => path_parent(output_dir),
    };
    parent.join(format!("{}.estage", archive_stem(archive_path))).clean()
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
            default_extract_work_dir(arch, Path::new("/out"), WorkDirLocation::BesideArchive),
            PathBuf::from("/data/foo.estage")
        );
        assert_eq!(
            default_extract_work_dir(
                arch,
                Path::new("/out/tree"),
                WorkDirLocation::BesideOutput
            ),
            PathBuf::from("/out/foo.estage")
        );
    }

    #[test]
    fn path_parent_keeps_filesystem_root() {
        assert_eq!(path_parent(Path::new("/foo.tar.gz")), Path::new("/"));
        assert_eq!(path_parent(Path::new("/")), Path::new("/"));
        assert_eq!(path_parent(Path::new("foo.tar.gz")), Path::new("."));
        assert_eq!(
            path_parent(Path::new("/data/foo.tar.gz")),
            Path::new("/data")
        );
    }

    #[test]
    fn resolve_path_to_abs_path_absolutizes_and_cleans() {
        let base = Path::new("/data/base");
        assert_eq!(
            resolve_path_to_abs_path(Path::new("/abs/x.tar"), base),
            PathBuf::from("/abs/x.tar")
        );
        assert_eq!(
            resolve_path_to_abs_path(Path::new("/a/../b/x.tar"), base),
            PathBuf::from("/b/x.tar")
        );
        assert_eq!(
            resolve_path_to_abs_path(Path::new("rel/y.tar"), base),
            PathBuf::from("/data/base/rel/y.tar")
        );
        assert_eq!(
            resolve_path_to_abs_path(Path::new("./rel/../z.tar"), base),
            PathBuf::from("/data/base/z.tar")
        );
    }

    #[test]
    fn resolve_cwd_resolves_existing_dir() {
        let cwd = std::env::current_dir().expect("cwd");
        assert_eq!(resolve_cwd(None).expect("none"), cwd);
        assert!(cwd.is_dir());
        assert_eq!(
            resolve_cwd(Some(cwd.as_path())).expect("abs"),
            cwd.clone()
        );

        // Nonexistent directory fails.
        let missing = cwd.join("__definitely_missing_dir__");
        let err = resolve_cwd(Some(&missing)).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    #[test]
    fn validate_dir_and_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let d = dir.path();
        assert!(validate_dir(d, "--dir").is_ok());
        let file = d.join("f");
        std::fs::write(&file, b"x").expect("write");
        assert!(validate_file(&file, "--file").is_ok());
        assert!(validate_dir(&file, "--dir").is_err(), "a file is not a dir");
        assert!(validate_file(&d, "--file").is_err(), "a dir is not a file");
        assert!(validate_dir(&d.join("missing"), "--dir").is_err());
        assert!(validate_file(&d.join("missing"), "--file").is_err());
    }

    #[test]
    fn parse_memlimit_suffixes_and_percent() {
        use super::*;
        assert_eq!(parse_memlimit("1024").unwrap(), 1024);
        assert_eq!(parse_memlimit("4KiB").unwrap(), 4 * 1024);
        assert_eq!(parse_memlimit("4K").unwrap(), 4 * 1000);
        assert_eq!(parse_memlimit("4MiB").unwrap(), 4 * 1024 * 1024);
        assert_eq!(parse_memlimit("4M").unwrap(), 4 * 1000 * 1000);
        assert_eq!(parse_memlimit("4GiB").unwrap(), 4 * 1024 * 1024 * 1024u64);
        assert_eq!(parse_memlimit("4G").unwrap(), 4 * 1000 * 1000 * 1000u64);
        // Percent of the physical RAM this test runs on.
        if let Some(ram) = physical_ram_bytes() {
            assert_eq!(parse_memlimit("50%").unwrap(), ram * 50 / 100);
        }
        assert!(matches!(parse_memlimit("0%"), Err(Error::Config(_))));
        assert!(matches!(parse_memlimit("101%"), Err(Error::Config(_))));
        assert!(matches!(parse_memlimit("banana"), Err(Error::Config(_))));
        assert!(matches!(
            parse_memlimit("999999999999999999999GiB"),
            Err(Error::Config(_))
        ));
    }

    #[test]
    fn archive_phase_next_respects_eager_filter() {
        use super::PipelinePhase as P;
        // eager: inventory -> filter -> hash -> dedup -> sparsify -> stage -> archive -> done.
        let chain_eager = [
            P::Inventory, P::Filter, P::Hash, P::Dedup,
            P::Sparsify, P::Stage, P::Archive, P::Done,
        ];
        for (i, phase) in chain_eager.iter().enumerate() {
            assert_eq!(phase.next(true), chain_eager.get(i + 1).copied(), "{phase:?}");
        }
        // lazy: inventory -> hash -> filter -> dedup -> ...
        assert_eq!(P::Inventory.next(false), Some(P::Hash));
        assert_eq!(P::Hash.next(false), Some(P::Filter));
        assert_eq!(P::Filter.next(false), Some(P::Dedup));
        assert_eq!(P::Done.next(false), None);
    }

    #[test]
    fn extract_phase_next_chain() {
        use super::ExtractPipelinePhase as E;
        let chain = [
            E::ScanTar, E::Filter, E::Rehash, E::PlacementPrologue,
            E::Place, E::Permissions, E::Cleanup, E::Done,
        ];
        for (i, phase) in chain.iter().enumerate() {
            assert_eq!(phase.next(), chain.get(i + 1).copied(), "{phase:?}");
        }
    }

    #[test]
    fn phase_as_str_parse_roundtrip() {
        for (phase, name) in [
            (PipelinePhase::Inventory, "inventory"),
            (PipelinePhase::Hash, "hash"),
            (PipelinePhase::Filter, "filter"),
            (PipelinePhase::Dedup, "dedup"),
            (PipelinePhase::Sparsify, "sparsify"),
            (PipelinePhase::Stage, "stage"),
            (PipelinePhase::Archive, "archive"),
            (PipelinePhase::Done, "done"),
        ] {
            assert_eq!(phase.as_str(), name);
            assert_eq!(PipelinePhase::parse(name).unwrap(), phase);
        }
        assert!(PipelinePhase::parse("bogus").is_err());
        for (phase, name) in [
            (ExtractPipelinePhase::ScanTar, "scan_tar"),
            (ExtractPipelinePhase::Filter, "filter"),
            (ExtractPipelinePhase::Rehash, "rehash"),
            (ExtractPipelinePhase::PlacementPrologue, "placement_prologue"),
            (ExtractPipelinePhase::Place, "place"),
            (ExtractPipelinePhase::Permissions, "permissions"),
            (ExtractPipelinePhase::Cleanup, "cleanup"),
            (ExtractPipelinePhase::Done, "done"),
        ] {
            assert_eq!(phase.as_str(), name);
            assert_eq!(ExtractPipelinePhase::parse(name).unwrap(), phase);
        }
        assert!(ExtractPipelinePhase::parse("bogus").is_err());
    }
}