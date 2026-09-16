use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cli::ArchiveArgs;
use crate::common::files::directory_roots_overlap;
use crate::common::start::StartPolicy;
use crate::error::{Error, Result};

use super::compression::{CompressionSettings, resolve_compression};
use super::paths::{PathLayout, PathSource};
use super::process::{CleanupSettings, ExitAfterStage, ProcessOptions};
use super::{
    default_archive_work_dir, resolve_cwd, resolve_path_to_abs_path, validate_dir, validate_file,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputOptions {
    pub input_dirs: Vec<PathSource>,
    pub files_from: Vec<PathBuf>,
    pub files_from_null: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexingOptions {
    pub no_recursion: bool,
    pub dereference: bool,
    pub one_file_system: bool,
    pub no_hardlink_detection: bool,
    pub no_strict_separation: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterOptions {
    pub exclude_patterns: Vec<String>,
    pub include_patterns: Vec<String>,
    pub exclude_from: Vec<PathBuf>,
    pub include_from: Vec<PathBuf>,
    pub anchored: bool,
    pub ignore_case: bool,
    pub eager_filter: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureOptions {
    pub do_xattrs: bool,
    pub do_posix_acl: bool,
    pub do_selinux: bool,
    pub numeric_ids_only: bool,
    /// Symbolic mode changes to apply at extraction (GNU tar `--mode`).
    pub mode: Option<String>,
    /// sed-style name transform to apply at extraction (GNU tar `--transform`).
    pub transform: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerPolicy {
    pub owner: Option<String>,
    pub owner_map: Option<PathBuf>,
    pub group: Option<String>,
    pub group_map: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SparseOptions {
    pub sparsify: bool,
    pub page_size: usize,
    pub min_pages: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivePipelineOptions {
    pub no_dedup: bool,
    pub retry_missing_sha: bool,
    pub write_archive_footer: bool,
    pub clear_archive_meta: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Build from CLI args, optionally inheriting `base` where the args do not
    /// define a value (Option-B merge: a field whose arg-derived value matches the
    /// default is "not defined" and inherits from `base`).
    ///
    /// Pair validation and the capture-umbrella base selection happen first:
    /// `--no-capture-all-metadata` selects the EXCLUDE preset, otherwise the
    /// INCLUDE preset (archive default = capture everything).
    pub fn build(args: &ArchiveArgs, base: Option<&ArchiveConfig>) -> Result<Self> {
        validate_capture_pairs(args)?;
        let candidate = Self::try_from(args)?;
        match base {
            None => Ok(candidate),
            Some(base) => Ok(candidate.merge_over(base, &DEFAULT_CONFIG)),
        }
    }

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

        // TODO rethink accepted roots.
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

        // TODO: call resolve_xz_threads to ensure we have sufficient resources.

        if args.page_size == 0 {
            return Err(Error::Config("page_size must be greater than 0".into()));
        }

        // Validate early so a bad `--mode` fails before any work dir is created.
        let mode_changes: Option<String> = match &args.mode {
            Some(changes) => {
                crate::common::perms::parse_mode_changes(changes)?;
                Some(changes.clone())
            }
            None => None,
        };

        // Validate early so a bad `--transform` fails before any work dir is
        // created. Only validated and stored; never applied at archive time.
        let transform: Option<String> = match &args.transform {
            Some(expr) => {
                crate::common::transform::parse_transform_expr(expr)?;
                Some(expr.clone())
            }
            None => None,
        };

        let start_policy = StartPolicy::create_or_fresh(args.fresh);
        let jobs = args.jobs.unwrap_or_else(num_cpus::get);
        let io_jobs = args.io_jobs.unwrap_or_else(num_cpus::get);

        // Archive default: capture everything. `--no-capture-all-metadata` flips the
        // whole group off; per-bit `--x`/`--no-x` override on top (explicit wins).
        let capture_all = !args.no_capture_all_metadata;

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
                do_xattrs: resolve_capture_bit(args.no_xattrs,
                                               args.xattrs,
                                               capture_all),
                do_posix_acl: resolve_capture_bit(args.no_acls,
                                                  args.acls,
                                                  capture_all),
                do_selinux: resolve_capture_bit(args.no_selinux,
                                                args.selinux,
                                                capture_all),
                numeric_ids_only: resolve_capture_bit(args.numeric_ids_only,
                                                      args.resolve_numeric_ids,
                                                      capture_all),
                mode: mode_changes,
                transform,
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
                io_jobs,
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

impl ArchiveConfig {
    /// Option-B merge: fields whose candidate value matches `default` are "not
    /// defined" and inherit from `base`; everything else comes from the candidate.
    fn merge_over(&self, base: &Self, default: &Self) -> Self {
        Self {
            paths: self.paths.clone(),
            inputs: merge_pick(&self.inputs, &base.inputs, &default.inputs),
            indexing: merge_pick(&self.indexing, &base.indexing, &default.indexing),
            filter: merge_pick(&self.filter, &base.filter, &default.filter),
            capture: merge_pick(&self.capture, &base.capture, &default.capture),
            owner_policy: merge_pick(&self.owner_policy, &base.owner_policy, &default.owner_policy),
            sparse: merge_pick(&self.sparse, &base.sparse, &default.sparse),
            compression: merge_pick(&self.compression, &base.compression, &default.compression),
            process: ProcessOptions {
                start_policy: self.process.start_policy,
                jobs: merge_pick_u(self.process.jobs, base.process.jobs, default.process.jobs),
                io_jobs: merge_pick_u(self.process.io_jobs, base.process.io_jobs, default.process.io_jobs),
                fail_fast: merge_pick_b(self.process.fail_fast, base.process.fail_fast, default.process.fail_fast),
                no_errors: merge_pick_b(self.process.no_errors, base.process.no_errors, default.process.no_errors),
                cleanup: self.process.cleanup,
                exit_after_stage: self.process.exit_after_stage,
            },
            pipeline: merge_pick(&self.pipeline, &base.pipeline, &default.pipeline),
        }
    }
}

fn merge_pick<T: PartialEq + Clone>(cand: &T, base: &T, default: &T) -> T {
    if cand == default { base.clone() } else { cand.clone() }
}

fn merge_pick_u(cand: usize, base: usize, default: usize) -> usize {
    if cand == default { base } else { cand }
}

fn merge_pick_b(cand: bool, base: bool, default: bool) -> bool {
    if cand == default { base } else { cand }
}

/// Resolve one capture bit: explicit `--no-x` wins, then `--x`, then the base
/// (INCLUDE = `true` when `capture_all`, EXCLUDE = `false`).
fn resolve_capture_bit(no: bool, yes: bool, base: bool) -> bool {
    debug_assert!(!(no && yes), "PRECONDITION FAILED: no and yes prohibited.");
    if no {
        false
    } else if yes {
        true
    } else {
        base
    }
}

/// Reject contradictory pairs like `--acls --no-acls` before building the config.
fn validate_capture_pairs(args: &ArchiveArgs) -> Result<()> {
    let pairs = [
        (args.acls, args.no_acls, "--acls", "--no-acls"),
        (args.xattrs, args.no_xattrs, "--xattrs", "--no-xattrs"),
        (args.selinux, args.no_selinux, "--selinux", "--no-selinux"),
        (args.numeric_ids_only, args.resolve_numeric_ids, "--numeric-ids-only", "--resolve-numeric-ids"),
    ];
    for (yes, no, yes_flag, no_flag) in pairs {
        if yes && no {
            return Err(Error::Config(format!(
                "conflicting flags {} and {}; pick one",
                yes_flag, no_flag
            )));
        }
    }
    Ok(())
}

/// Archive config with every optional bit at its "off" default. Used as the
/// merge baseline: fields equal to `DEFAULT_CONFIG` count as unset during a
/// config merge.
const DEFAULT_CONFIG: ArchiveConfig = ArchiveConfig {
    paths: PathLayout {
        archive_path: PathBuf::new(),
        directory: PathBuf::new(),
        work_dir: PathBuf::new(),
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
        do_xattrs: false,
        do_posix_acl: false,
        do_selinux: false,
        mode: None,
        transform: None,
    },
    owner_policy: OwnerPolicy {
        owner: None,
        owner_map: None,
        group: None,
        group_map: None,
    },
    sparse: SparseOptions {
        sparsify: false,
        page_size: 0,
        min_pages: 0,
    },
    compression: CompressionSettings {
        format: super::compression::CompressionFormat::None,
        level: 0,
        xz_extreme: false,
        memlimit_compress: None,
    },
    process: ProcessOptions {
        start_policy: StartPolicy::Create,
        jobs: 0,
        io_jobs: 0,
        fail_fast: false,
        no_errors: false,
        cleanup: CleanupSettings { keep_db: false, keep_stage: false },
        exit_after_stage: None,
    },
    pipeline: ArchivePipelineOptions {
        no_dedup: false,
        retry_missing_sha: false,
        write_archive_footer: true,
        clear_archive_meta: false,
        numeric_ids_only: false,
    },
};
