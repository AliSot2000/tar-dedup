use std::path::{Path, PathBuf};

use crate::cli::ArchiveArgs;
use crate::common::files::directory_roots_overlap;
use crate::common::start::StartPolicy;
use crate::error::{Error, Result};

use super::compression::{resolve_compression, CompressionSettings};
use super::paths::{PathLayout, PathSource};
use super::process::{CleanupSettings, ExitAfterStage, ProcessOptions};
use super::{
    default_archive_work_dir, resolve_cwd, resolve_path_to_abs_path, validate_dir, validate_file,
};

#[derive(Debug, Clone)]
pub struct InputOptions {
    pub input_dirs: Vec<PathSource>,
    pub files_from: Vec<PathBuf>,
    pub files_from_null: bool,
}

#[derive(Debug, Clone)]
pub struct IndexingOptions {
    pub no_recursion: bool,
    pub dereference: bool,
    pub one_file_system: bool,
    pub no_hardlink_detection: bool,
    pub no_strict_separation: bool,
}

#[derive(Debug, Clone)]
pub struct FilterOptions {
    pub exclude_patterns: Vec<String>,
    pub include_patterns: Vec<String>,
    pub exclude_from: Vec<PathBuf>,
    pub include_from: Vec<PathBuf>,
    pub anchored: bool,
    pub ignore_case: bool,
    pub eager_filter: bool,
}

#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub do_xattrs: bool,
    pub do_posix_acl: bool,
    pub do_selinux: bool,
}

#[derive(Debug, Clone)]
pub struct OwnerPolicy {
    pub owner: Option<String>,
    pub owner_map: Option<PathBuf>,
    pub group: Option<String>,
    pub group_map: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct SparseOptions {
    pub sparsify: bool,
    pub page_size: usize,
    pub min_pages: u64,
}

#[derive(Debug, Clone)]
pub struct ArchivePipelineOptions {
    pub no_dedup: bool,
    pub retry_missing_sha: bool,
    pub write_archive_footer: bool,
    pub clear_archive_meta: bool,
}

#[derive(Debug, Clone)]
pub struct ArchiveConfig {
    pub paths: PathLayout,
    pub inputs: InputOptions,
    pub indexing: IndexingOptions,
    pub filter: FilterOptions,
    pub capture: CaptureOptions,
    pub owner_policy: OwnerPolicy,
    pub sparse: SparseOptions,
    pub compression: CompressionSettings,
    pub process: ProcessOptions,
    pub pipeline: ArchivePipelineOptions,
}

impl ArchiveConfig {
    pub fn try_from(args: &ArchiveArgs) -> Result<Self> {
        let directory = resolve_cwd(args.directory.as_deref())?;

        let archive_path = resolve_path_to_abs_path(&args.archive, &directory);
        if let Some(parent) = archive_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        let work_dir = match &args.work_dir {
            Some(path) => resolve_path_to_abs_path(path, &directory),
            None => default_archive_work_dir(&archive_path),
        };
        std::fs::create_dir_all(&work_dir).map_err(|e| Error::io(&work_dir, e))?;

        if args.input_dirs.is_empty() && args.files_from.is_empty() {
            return Err(Error::Config(
                "at least one of `-i`/`--input-dir` or `-T`/`--files-from` is required".into(),
            ));
        }

        let mut input_dirs = Vec::with_capacity(args.input_dirs.len());
        let mut accepted_roots: Vec<PathBuf> = Vec::with_capacity(args.input_dirs.len());
        for dir in &args.input_dirs {
            let resolved = resolve_path_to_abs_path(dir, &directory);
            validate_dir(&resolved, "--input-dir")?;
            if let Some(existing) = accepted_roots
                .iter()
                .find(|root| directory_roots_overlap(root, &resolved, args.no_recursion))
            {
                if !args.no_strict_separation {
                    return Err(Error::Config(format!(
                        "input directory `{}` overlaps `{}`; use `--no-strict-separation` to walk anyway",
                        resolved.display(),
                        existing.display()
                    )));
                }
            }
            accepted_roots.push(resolved.clone());
            input_dirs.push(PathSource {
                original_path: dir.to_path_buf(),
                absolute_path: resolved,
            });
        }

        let files_from: Vec<PathBuf> = args
            .files_from
            .iter()
            .map(|p| {
                if p.as_os_str() == "-" {
                    Ok(PathBuf::from("-"))
                } else {
                    let resolved = resolve_path_to_abs_path(p, &directory);
                    validate_file(resolved.as_ref(), "--from-file")?;
                    Ok(resolved)
                }
            })
            .collect::<Result<_>>()?;

        let format = resolve_compression(&args.compression, &archive_path)?;
        let compression = CompressionSettings::from_archive_args(format, args)?;

        let exclude_from: Vec<PathBuf> = args
            .exclude_from
            .iter()
            .map(|p| {
                let resolved = resolve_path_to_abs_path(p, &directory);
                validate_file(&resolved, "--exclude-from")?;
                Ok(resolved)
            })
            .collect::<Result<_>>()?;
        let include_from: Vec<PathBuf> = args
            .include_from
            .iter()
            .map(|p| {
                let resolved = resolve_path_to_abs_path(p, &directory);
                validate_file(&resolved, "--exclude-from")?;
                Ok(resolved)
            })
            .collect::<Result<_>>()?;

        if args.exclude_vcs || args.exclude_vcs_ignores {
            return Err(Error::Config(
                "--exclude-vcs / --exclude-vcs-ignores are not implemented yet".into(),
            ));
        }

        let owner_map: Option<PathBuf> = args
            .owner_map
            .as_ref()
            .map(|p| -> Result<PathBuf> {
                let resolved = resolve_path_to_abs_path(p, &directory);
                validate_file(&resolved, "--owner-map")?;
                Ok(resolved)
            })
            .transpose()?;
        let group_map: Option<PathBuf> = args
            .group_map
            .as_ref()
            .map(|p| -> Result<PathBuf> {
                let resolved = resolve_path_to_abs_path(p, &directory);
                validate_file(&resolved, "--group-map")?;
                Ok(resolved)
            })
            .transpose()?;

        if args.page_size == 0 {
            return Err(Error::Config("page_size must be greater than 0".into()));
        }

        let start_policy = StartPolicy::create_or_fresh(args.fresh);
        let jobs = args.jobs.unwrap_or_else(num_cpus::get);

        Ok(Self {
            paths: PathLayout {
                archive_path,
                directory,
                work_dir,
            },
            inputs: InputOptions {
                input_dirs,
                files_from,
                files_from_null: args.null,
            },
            indexing: IndexingOptions {
                no_recursion: args.no_recursion,
                dereference: args.dereference,
                one_file_system: args.one_file_system,
                no_hardlink_detection: args.no_hardlink_detection,
                no_strict_separation: args.no_strict_separation,
            },
            filter: FilterOptions {
                exclude_patterns: args.exclude.clone(),
                include_patterns: args.include.clone(),
                exclude_from,
                include_from,
                anchored: args.anchored,
                ignore_case: args.ignore_case,
                eager_filter: !args.lazy_filter,
            },
            capture: CaptureOptions {
                do_xattrs: args.xattrs,
                do_posix_acl: args.acls,
                do_selinux: args.selinux,
            },
            owner_policy: OwnerPolicy {
                owner: args.owner.clone(),
                owner_map,
                group: args.group.clone(),
                group_map,
            },
            sparse: SparseOptions {
                sparsify: args.sparsify,
                page_size: args.page_size,
                min_pages: args.min_pages,
            },
            compression,
            process: ProcessOptions {
                start_policy,
                jobs,
                fail_fast: args.fail_fast,
                no_errors: args.no_errors,
                cleanup: CleanupSettings::from_flags(args.keep_db, args.keep_stage),
                exit_after_stage: args.exit_after_stage.map(ExitAfterStage::from),
            },
            pipeline: ArchivePipelineOptions {
                no_dedup: args.no_dedup,
                retry_missing_sha: args.retry_missing_sha,
                write_archive_footer: true,
                clear_archive_meta: false,
            },
        })
    }

    /// Minimal config for `resume` when archive runtime state is present.
    pub fn for_resume(work_dir: PathBuf, jobs: usize, exit_after_stage: Option<ExitAfterStage>) -> Self {
        Self {
            paths: PathLayout {
                archive_path: PathBuf::new(),
                directory: PathBuf::new(),
                work_dir,
            },
            inputs: InputOptions {
                input_dirs: Vec::new(),
                files_from: Vec::new(),
                files_from_null: false,
            },
            indexing: IndexingOptions {
                no_recursion: false,
                dereference: false,
                one_file_system: false,
                no_hardlink_detection: false,
                no_strict_separation: false,
            },
            filter: FilterOptions {
                exclude_patterns: Vec::new(),
                include_patterns: Vec::new(),
                exclude_from: Vec::new(),
                include_from: Vec::new(),
                anchored: false,
                ignore_case: false,
                eager_filter: false,
            },
            capture: CaptureOptions {
                do_xattrs: true,
                do_posix_acl: true,
                do_selinux: true,
            },
            owner_policy: OwnerPolicy {
                owner: None,
                owner_map: None,
                group: None,
                group_map: None,
            },
            sparse: SparseOptions {
                sparsify: false,
                page_size: 4096,
                min_pages: 0,
            },
            compression: CompressionSettings {
                format: super::compression::CompressionFormat::None,
                level: 0,
                xz_extreme: false,
                memlimit_compress: None,
            },
            process: ProcessOptions {
                start_policy: StartPolicy::Resume,
                jobs,
                fail_fast: false,
                no_errors: false,
                cleanup: CleanupSettings::from_flags(false, false),
                exit_after_stage,
            },
            pipeline: ArchivePipelineOptions {
                no_dedup: false,
                retry_missing_sha: false,
                write_archive_footer: true,
                clear_archive_meta: false,
            },
        }
    }
}

impl super::WorkLayout for ArchiveConfig {
    fn paths(&self) -> &PathLayout {
        &self.paths
    }

    fn cleanup(&self) -> &CleanupSettings {
        &self.process.cleanup
    }

    fn kept_db_parent<'a>(&'a self, mode: super::CleanupMode) -> &'a Path {
        match mode {
            super::CleanupMode::Archive => super::path_parent(&self.paths.archive_path),
            super::CleanupMode::Extract => self.paths.extraction_root(),
        }
    }
}
