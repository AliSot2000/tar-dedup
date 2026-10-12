use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cli::{ArchiveArgs, ArchiveOrdering, DedupMode};
use crate::common::files::directory_roots_overlap;
use crate::common::perms::parse_owner_group_args;
use crate::common::start::StartPolicy;
use crate::config::filter::FilterOptions;
use crate::error::{Error, Result};

use super::Validate;
use super::common::{merge_pick_clone, merge_pick_copy, resolve_bool_flag};
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

impl Default for InputOptions {
    fn default() -> Self {
        Self {
            input_dirs: Vec::new(),
            files_from: Vec::new(),
            files_from_null: false,
        }
    }
}

impl Validate for InputOptions {
    fn validate(&self) -> Result<()> {
        if self.input_dirs.is_empty() && self.files_from.is_empty() {
            return Err(Error::Config(
                "at least one of `-i`/`--input-dir` or `-T`/`--files-from` is required".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexingOptions {
    pub no_recursion: bool,
    pub dereference: bool,
    pub one_file_system: bool,
    pub no_hardlink_detection: bool,
    pub no_strict_separation: bool,
}

impl Default for IndexingOptions {
    fn default() -> Self {
        Self {
            no_recursion: false,
            dereference: false,
            one_file_system: false,
            no_hardlink_detection: false,
            no_strict_separation: false,
        }
    }
}

impl Validate for IndexingOptions {
    // INFO: Added stub to prevent later changes from being omitted due to not calling the function.
    fn validate(&self) -> Result<()> {
        Ok(())
    }
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

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            do_xattrs: true,
            do_posix_acl: true,
            do_selinux: true,
            numeric_ids_only: false,
            mode: None,
            transform: None,
        }
    }
}

impl Validate for CaptureOptions {
    fn validate(&self) -> Result<()> {
        if let Some(changes) = self.mode.as_ref() {
            crate::common::perms::parse_mode_changes(changes)?;
        }
        if let Some(transform) = self.transform.as_ref() {
            crate::common::transform::parse_transform_expr(transform)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerPolicy {
    pub owner: Option<String>,
    pub owner_map: Option<PathBuf>,
    pub group: Option<String>,
    pub group_map: Option<PathBuf>,
}

impl Default for OwnerPolicy {
    fn default() -> Self {
        Self {
            owner: None,
            owner_map: None,
            group: None,
            group_map: None,
        }
    }
}

impl Validate for OwnerPolicy {
    fn validate(&self) -> Result<()> {
        // INFO: Parsing will be called twice but since files expected to be small, overhead from
        //  parsing twice should be ok. Using the parser for validation.
        match parse_owner_group_args(
            self.owner.as_deref(),
            self.owner_map.as_deref(),
            self.group.as_deref(),
            self.group_map.as_deref()) {
            Ok(_) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SparseOptions {
    pub sparsify: bool,
    pub page_size: usize,
    pub min_pages: u64,
}

impl Default for SparseOptions {
    fn default() -> Self {
        Self {
            sparsify: false,
            page_size: 4096,
            min_pages: 0,
        }
    }
}

// TODO merge into single file
impl Validate for SparseOptions {
    fn validate(&self) -> Result<()> {
        if self.sparsify && self.page_size == 0 {
            return Err(Error::Config(
                "--page-size must be greater than 0 when `--sparsify` is set".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchivePipelineOptions {
    pub dedup_mode: DedupMode,
    /// Tar payload ordering (ext,size,id / ext,name,size,id / id).
    pub order: ArchiveOrdering,
    /// Content-defined chunking (schemas exist; implementation not yet wired).
    pub use_shards: bool,
    /// Maximum shard size for content-defined chunking.
    pub max_shard_size: u64,
    pub write_archive_footer: bool,
}

impl Default for ArchivePipelineOptions {
    fn default() -> Self {
        Self {
            dedup_mode: DedupMode::Regular,
            order: ArchiveOrdering::ExtSizeId,
            use_shards: false,
            max_shard_size: 1024 * 1024 * 5,
            write_archive_footer: true
        }
    }
}

impl Validate for ArchivePipelineOptions {
    fn validate(&self) -> Result<()> {
        if self.use_shards {
            return Err(Error::Config(
                "--use-shards (content-defined chunking) is not implemented yet".into(),
            ));
        }
        if self.max_shard_size > 0 && !self.use_shards {
            return Err(Error::Config(
                "--max-shard-size requires --use-shards".into(),
            ));
        }
        Ok(())
    }
}


#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
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
        let candidate = Self::try_from(args)?;
        // Validation happens on the merged result: a field alone may be valid but
        // invalid once combined with an inherited base value (see `Validate`).
        let merged = match base {
            None => candidate,
            Some(base) => candidate.merge_over(base, &ArchiveConfig::default()),
        };
        merged.validate()?;
        Ok(merged)
    }

    /// Option-B merge: fields whose candidate value matches `default` are "not
    /// defined" and inherit from `base`; everything else comes from the candidate.
    fn merge_over(&self, base: &Self, default: &Self) -> Self {
        Self {
            paths: self.paths.clone(),
            inputs: merge_pick_clone(&self.inputs, &base.inputs, &default.inputs),
            indexing: merge_pick_clone(&self.indexing, &base.indexing, &default.indexing),
            filter: merge_pick_clone(&self.filter, &base.filter, &default.filter),
            capture: merge_pick_clone(&self.capture, &base.capture, &default.capture),
            owner_policy: merge_pick_clone(&self.owner_policy, &base.owner_policy, &default.owner_policy),
            sparse: merge_pick_clone(&self.sparse, &base.sparse, &default.sparse),
            compression: merge_pick_clone(&self.compression, &base.compression, &default.compression),
            process: ProcessOptions {
                start_policy: self.process.start_policy,
                jobs: merge_pick_copy(self.process.jobs, base.process.jobs, default.process.jobs),
                io_jobs: merge_pick_copy(self.process.io_jobs, base.process.io_jobs, default.process.io_jobs),
                auto_flush_limit: merge_pick_copy(self.process.auto_flush_limit, base.process.auto_flush_limit, default.process.auto_flush_limit),
                batch_size: merge_pick_copy(self.process.batch_size, base.process.batch_size, default.process.batch_size),
                work_capacity: merge_pick_copy(self.process.work_capacity, base.process.work_capacity, default.process.work_capacity),
                out_capacity: merge_pick_copy(self.process.out_capacity, base.process.out_capacity, default.process.out_capacity),
                feed_chunk: merge_pick_copy(self.process.feed_chunk, base.process.feed_chunk, default.process.feed_chunk),
                drain_chunk: merge_pick_copy(self.process.drain_chunk, base.process.drain_chunk, default.process.drain_chunk),
                fail_fast: merge_pick_copy(self.process.fail_fast, base.process.fail_fast, default.process.fail_fast),
                no_errors: merge_pick_copy(self.process.no_errors, base.process.no_errors, default.process.no_errors),
                force: merge_pick_copy(self.process.force, base.process.force, default.process.force),
                relax_utf8: merge_pick_copy(self.process.relax_utf8, base.process.relax_utf8, default.process.relax_utf8),
                progress_mode: merge_pick_copy(self.process.progress_mode, base.process.progress_mode, default.process.progress_mode),
                cleanup: self.process.cleanup,
                exit_after_stage: self.process.exit_after_stage,
            },
            pipeline: merge_pick_clone(&self.pipeline, &base.pipeline, &default.pipeline),
        }
    }

    pub fn try_from(args: &ArchiveArgs) -> Result<Self> {
        validate_capture_pairs(args)?;
        let c_directory = resolve_cwd(args.directory.as_deref())?;

        // Resolve archive path and create parents.
        let c_archive_path = resolve_path_to_abs_path(&args.archive, &c_directory);
        if let Some(parent) = c_archive_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }

        // Resolve work dir and create it and parents
        let c_work_dir = match &args.work_dir {
            Some(path) => resolve_path_to_abs_path(path, &c_directory),
            None => default_archive_work_dir(&c_archive_path),
        };
        std::fs::create_dir_all(&c_work_dir).map_err(|e| Error::io(&c_work_dir, e))?;

        // Input dirs && files from
        let mut input_dirs = Vec::with_capacity(args.input_dirs.len());
        // INFO: Scope to drop accepted_roots again.
        {
            let mut accepted_roots: Vec<PathBuf> = Vec::with_capacity(args.input_dirs.len());
            for dir in &args.input_dirs {
                let c_resolved = resolve_path_to_abs_path(dir, &c_directory);
                validate_dir(&c_resolved, "--input-dir")?;
                if let Some(existing) = accepted_roots
                    .iter()
                    .find(|root| directory_roots_overlap(
                        root, &c_resolved, args.no_recursion)) {

                    if !args.no_strict_separation {
                        return Err(Error::Config(format!(
                            "input directory `{}` overlaps `{}`; use `--no-strict-separation` to \
                            walk anyway",
                            c_resolved.display(),
                            existing.display()
                        )));
                    }
                }
                accepted_roots.push(c_resolved.clone());
                input_dirs.push(PathSource {
                    // INFO: For reproducibility purposes we, we store the original_path not cleaned
                    //  and consumers of original_path need sanitize it.
                    original_path: dir.to_path_buf(),
                    absolute_path: c_resolved,
                });
            }
        }
        let c_files_from: Vec<PathBuf> = args
            .files_from
            .iter()
            .map(|p| {
                if p.as_os_str() == "-" {
                    Ok(PathBuf::from("-"))
                } else {
                    let c_resolved = resolve_path_to_abs_path(p, &c_directory);
                    validate_file(c_resolved.as_ref(), "--from-file")?;
                    Ok(c_resolved)
                }
            })
            .collect::<Result<_>>()?;

        // Compression
        let format = resolve_compression(&args.compression, &c_archive_path)?;
        let compression = CompressionSettings::from_archive_args(format, args)?;

        // Include & Exclude files
        let c_exclude_from: Vec<PathBuf> = args
            .exclude_from
            .iter()
            .map(|p| {
                let resolved = resolve_path_to_abs_path(p, &c_directory);
                validate_file(&resolved, "--exclude-from")?;
                Ok(resolved)
            })
            .collect::<Result<_>>()?;
        let c_include_from: Vec<PathBuf> = args
            .include_from
            .iter()
            .map(|p| {
                let resolved = resolve_path_to_abs_path(p, &c_directory);
                validate_file(&resolved, "--exclude-from")?;
                Ok(resolved)
            })
            .collect::<Result<_>>()?;

        if args.exclude_vcs || args.exclude_vcs_ignores {
            return Err(Error::Config(
                "--exclude-vcs / --exclude-vcs-ignores are not implemented yet".into(),
            ));
        }

        let c_owner_map: Option<PathBuf> = args
            .owner_map
            .as_ref()
            .map(|p| -> Result<PathBuf> {
                let c_resolved = resolve_path_to_abs_path(p, &c_directory);
                validate_file(&c_resolved, "--owner-map")?;
                Ok(c_resolved)
            })
            .transpose()?;
        let c_group_map: Option<PathBuf> = args
            .group_map
            .as_ref()
            .map(|p| -> Result<PathBuf> {
                let c_resolved = resolve_path_to_abs_path(p, &c_directory);
                validate_file(&c_resolved, "--group-map")?;
                Ok(c_resolved)
            })
            .transpose()?;

        let start_policy = StartPolicy::create_or_fresh(args.fresh);
        let jobs = args.jobs.unwrap_or_else(num_cpus::get);
        let io_jobs = args.io_jobs.unwrap_or_else(num_cpus::get);

        // Archive default: capture everything. `--no-capture-all-metadata` flips the
        // whole group off; per-bit `--x`/`--no-x` override on top (explicit wins).
        let capture_all = !args.no_capture_all_metadata;

        Ok(Self {
            paths: PathLayout {
                archive_path: c_archive_path,
                directory: c_directory.clone(),
                output_dir: c_directory,
                work_dir: c_work_dir,
            },
            inputs: InputOptions {
                input_dirs,
                files_from: c_files_from,
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
                exclude_from: c_exclude_from,
                include_from: c_include_from,
                anchored: args.anchored,
                ignore_case: args.ignore_case,
                no_unicode: args.no_unicode,
                eager_filter: !args.lazy_filter,
            },
            capture: CaptureOptions {
                do_xattrs: resolve_bool_flag(args.no_xattrs,
                                             args.xattrs,
                                             capture_all),
                do_posix_acl: resolve_bool_flag(args.no_acls,
                                                args.acls,
                                                capture_all),
                do_selinux: resolve_bool_flag(args.no_selinux,
                                              args.selinux,
                                              capture_all),
                numeric_ids_only: resolve_bool_flag(args.numeric_ids_only,
                                                    args.resolve_numeric_ids,
                                                    capture_all),
                mode: args.mode.clone(),
                transform: args.transform.clone(),
            },
            owner_policy: OwnerPolicy {
                owner: args.owner.clone(),
                owner_map: c_owner_map,
                group: args.group.clone(),
                group_map: c_group_map,
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
                auto_flush_limit: args.auto_flush_limit,
                batch_size: args.batch_size,
                work_capacity: args.work_capacity,
                out_capacity: args.out_capacity,
                feed_chunk: args.feed_chunk,
                drain_chunk: args.drain_chunk,
                fail_fast: args.fail_fast,
                no_errors: args.no_errors,
                force: false,
                relax_utf8: args.relax_utf8,
                progress_mode: args.progress_mode,
                cleanup: CleanupSettings::from_flags(args.keep_db, args.keep_stage),
                exit_after_stage: args.exit_after_stage.map(ExitAfterStage::from),
            },
            pipeline: ArchivePipelineOptions {
                dedup_mode: args.dedup_mode,
                order: args.ordering,
                use_shards: args.use_shards,
                max_shard_size: args.max_shard_size,
                write_archive_footer: true,
            },
        })
    }
}

impl Validate for ArchiveConfig {
    fn validate(&self) -> Result<()> {
        self.paths.validate()?;
        self.inputs.validate()?;
        self.indexing.validate()?;
        self.filter.validate()?;
        self.capture.validate()?;
        self.owner_policy.validate()?;
        self.sparse.validate()?;
        self.compression.validate_aux(self.process.jobs)?;
        self.process.validate()?;
        self.pipeline.validate()?;
        Ok(())
    }
}

impl super::WorkLayout for ArchiveConfig {
    fn paths(&self) -> &PathLayout {
        &self.paths
    }

    fn cleanup(&self) -> &CleanupSettings {
        &self.process.cleanup
    }

    fn kept_db_parent(&self, mode: super::CleanupMode) -> &Path {
        match mode {
            super::CleanupMode::Archive => super::path_parent(&self.paths.archive_path),
            super::CleanupMode::Extract => self.paths.extraction_root(),
        }
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