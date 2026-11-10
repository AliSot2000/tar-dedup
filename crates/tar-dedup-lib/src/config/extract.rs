use path_clean::PathClean;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::cli::{ConflictPolicy, ExtractArgs, HardLinkGrouping};
use crate::common::perms::{
    MapResolutionTarget, ModeSource, OwnerGroupSource, infer_same_owner, parse_mode_changes,
    parse_owner_group_args, validate_for_resolution_mode,
};
use crate::common::start::StartPolicy;
use crate::common::transform::{TransformSource, parse_transform_expr};
use crate::config::filter::FilterOptions;
use crate::error::{Error, Result};

use super::Validate;
use super::common::{merge_pick_clone, merge_pick_copy, resolve_bool_flag};
use super::compression::{CompressionFormat, resolve_compression};
use super::paths::PathLayout;
use super::process::{CleanupSettings, ExitAfterStage, ProcessOptions};
use super::{DbRetention, StageKeep};
use super::{
    archive_stem, default_extract_work_dir, resolve_cwd, resolve_path_to_abs_path, validate_file,
};

/// When per-entry JSON sidecar manifests are written beside extracted entries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum SidecarMode {
    /// Write a sidecar manifest for every placed entry.
    Always,
    /// Write a sidecar only where metadata application failed.
    #[default]
    OnError,
    /// Write no sidecar manifests.
    Never,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementOptions {
    pub absolute_names: bool,
    pub one_top_level: Option<PathBuf>,
    pub keep_dir_symlink: bool,
    pub unlink_first: bool,
    pub no_create_dir: bool,
    pub conflict_policy: ConflictPolicy,
    pub silent_conflicts: bool,
    pub remove_and_replace: bool,
    pub link_tree: bool,
    pub use_hard_links: bool,
    pub absolute_links: bool,
    pub clean_target: bool,
    pub link_source: Option<PathBuf>,
    pub no_reflink: bool,
    pub hard_link_grouping: HardLinkGrouping,
    pub recreate_none_file_entries: bool,
    pub lazy_delete_stage: bool,
    pub strip_common_prefix: bool,
}

impl Default for PlacementOptions {
    fn default() -> Self {
        Self {
            absolute_names: false,
            one_top_level: None,
            keep_dir_symlink: false,
            unlink_first: false,
            no_create_dir: false,
            conflict_policy: ConflictPolicy::Replace,
            silent_conflicts: false,
            remove_and_replace: false,
            link_tree: false,
            use_hard_links: false,
            absolute_links: false,
            clean_target: false,
            link_source: None,
            no_reflink: false,
            hard_link_grouping: HardLinkGrouping::Global,
            recreate_none_file_entries: false,
            lazy_delete_stage: false,
            strip_common_prefix: false,
        }
    }
}

impl Validate for PlacementOptions {
    fn validate(&self) -> Result<()> {
        if self.clean_target && self.no_create_dir {
            return Err(Error::Config(
                "--clean-target conflicts with --no-create-dirs".into(),
            ));
        }
        if self.absolute_links && !self.link_tree {
            tracing::warn!(
                "--absolute-links requires --link-tree to have an effect"
            );
        }
        if self.absolute_links && self.use_hard_links {
            tracing::warn!("--absolute-links has no effect with --hard-links");
        }
        if let Some(ls) = self.link_source.as_ref() {
            if !self.link_tree {
                tracing::warn!(
                    "--link-source requires --link-tree to have an effect"
                );
            }
            validate_name(ls, "--link-source")?;
        }
        if self.clean_target && self.one_top_level.is_none() {
            tracing::warn!("--clean-target has no effect without --one-top-level");
        }
        if let Some(otl) = self.one_top_level.as_ref() {
            validate_name(otl, "--one-top-level")?;
        }
        if self.absolute_names && matches!(self.hard_link_grouping, HardLinkGrouping::Source) {
            tracing::warn!(
                "Materializing with --absolute-names and per source hardlink recreation might \
                cause disjoint subgroups of hardlinks that formerly were a single hardlink."
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractAttributeOptions {
    pub restore_owner: bool,
    pub no_overwrite_dir: bool,
    pub overwrite_all_dir: bool,
    pub apply_atime: bool,
    pub apply_mtime: bool,
    pub no_xattrs: bool,
    pub no_acls: bool,
    pub no_selinux: bool,
    pub no_same_permissions: bool,
}

impl Default for ExtractAttributeOptions {
    fn default() -> Self {
        Self {
            restore_owner: false,
            no_overwrite_dir: false,
            overwrite_all_dir: false,
            apply_atime: false,
            apply_mtime: false,
            no_xattrs: false,
            no_acls: false,
            no_selinux: false,
            no_same_permissions: false,       
        }
    }
}

impl Validate for ExtractAttributeOptions {
    fn validate(&self) -> Result<()> {
        if self.no_overwrite_dir && self.overwrite_all_dir {
            tracing::warn!("--overwrite-all-dir is disabled with --no-overwrite-dir");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerGroupOptions {
    pub target: MapResolutionTarget,
    pub validate_maps: bool,
    pub same_owner: bool,
    pub apply_owner: bool,
    pub apply_group: bool,
}

impl Default for OwnerGroupOptions {
    fn default() -> Self {
        Self {
            target: MapResolutionTarget::NameId,
            validate_maps: false,
            same_owner: false,
            apply_owner: false,
            apply_group: false,            
        }
    }
}

impl Validate for OwnerGroupOptions {
    // INFO: Stub for the moment, so all sub structs have a validate method()
    fn validate(&self) -> Result<()> {
        if !self.same_owner && self.apply_owner {
            tracing::warn!("--apply-stored-owner-map has no effect without --same-owner");
        }
        if !self.same_owner && self.apply_group{
            tracing::warn!("--apply-stored-group-map has no effect without --same-owner");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanOptions {
    pub sparsify: bool,
    pub page_size: usize,
    /// Verify the stored archive's footer content sha1 before extracting. Default: on.
    pub verify_archive: bool,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            sparsify: false,
            page_size: 4096,
            verify_archive: true,
        }
    }
}

impl Validate for ScanOptions {
    fn validate(&self) -> Result<()> {
        if self.sparsify && self.page_size == 0 {
            return Err(Error::Config(
                "--page-size must be greater than 0 when `--sparsify` is set".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractConfig {
    pub paths: PathLayout,
    pub decompression: CompressionFormat,
    pub placement: PlacementOptions,
    pub attributes: ExtractAttributeOptions,
    pub scan: ScanOptions,
    /// Run the rehash (content verification) phase. Default: on.
    pub rehash: bool,
    pub process: ProcessOptions,
    pub owner_policy: OwnerGroupSource,
    pub owner_group: OwnerGroupOptions,
    pub mode_policy: ModeSource,
    pub strip_components: u32,
    pub transform_policy: TransformSource,
    pub filter: FilterOptions,
    /// When sidecar manifests are written for placed entries.
    pub sidecar: SidecarMode,
    /// Prefix sidecar names with a leading dot (`{name}.json` → `.{name}.json`).
    pub hidden_sidecar: bool,
    /// Product is a `--bypass-archive` directory (`-f DIR`): `tar-dedup.sqlite`
    /// + canonical payloads, extracted by validating the pre-extracted directory.
    pub bypass_archive: bool,
}

impl Default for ExtractConfig {
    /// Extract config with every optional bit at its "off" default. Used as the
    /// merge baseline: fields equal to `DEFAULT_CONFIG` count as unset during a
    /// config mergee
    fn default() -> Self {
        Self {
            paths: PathLayout::default(),
            decompression: CompressionFormat::None,
            placement: PlacementOptions::default(),
            attributes: ExtractAttributeOptions::default(),
            scan: ScanOptions::default(),
            rehash: true,
            process: ProcessOptions::default(),
            owner_policy: OwnerGroupSource::None,
            owner_group: OwnerGroupOptions::default(),
            mode_policy: ModeSource::None,
            strip_components: 0,
            transform_policy: TransformSource::None,
            filter: FilterOptions::default(),
            sidecar: SidecarMode::OnError,
            hidden_sidecar: false,
            bypass_archive: false,
        }
    }
}

impl super::WorkLayout for ExtractConfig {
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

impl Validate for ExtractConfig {
    fn validate(&self) -> Result<()> {
        self.paths.validate()?;
        self.placement.validate()?;
        self.attributes.validate()?;
        self.scan.validate()?;
        self.process.validate()?;
        self.owner_group.validate()?;
        self.filter.validate()?;

        // Sidecar folds (restrictor wins, warn-only like the no-overwrite-dir /
        // overwrite-all-dir pairing).
        if self.placement.link_tree && self.sidecar == SidecarMode::OnError {
            tracing::warn!(
                "--sidecar on-error has no effect with --link-tree (metadata applies to the \
                 `.sources` canonicals, no per-link errors exist); falling back to never"
            );
        }
        if self.process.no_errors && self.sidecar == SidecarMode::OnError {
            tracing::warn!(
                "--sidecar on-error has no effect with --no-errors (nothing is recorded); \
                 falling back to never"
            );
        }

        // A bypass product has no tar stream / footer sha1: content verification
        // is the only integrity guarantee, so `--no-rehash` cannot be honoured.
        // `--force` is the explicit "data may be invalid" escape hatch.
        if self.bypass_archive && !self.rehash && !self.process.force {
            return Err(Error::Config(
                "`--no-rehash` cannot be used with `--bypass-archive`: a bypass product has no \
                 tar stream sha1, so content rehash is the only integrity guarantee. \
                 Use --force to extract without it."
                    .into(),
            ));
        }
        Ok(())
    }
}

impl ExtractConfig {
    /// Build from CLI args, optionally inheriting `base` where the args do not
    /// define a value (Option-B merge: a field whose arg-derived value matches the
    /// default is "not defined" and inherits from `base`).
    ///
    /// Pair validation runs first; `--apply-all-metadata` selects the INCLUDE base
    /// (apply every bit) and the default EXCLUDE base applies none, with per-bit
    /// `--x`/`--no-x` flags overriding the base in either direction.
    pub fn build(args: &ExtractArgs, base: Option<&ExtractConfig>) -> Result<Self> {
        let candidate = Self::try_from(args)?;
        // Validation happens on the merged result: a field alone may be valid but
        // invalid once combined with an inherited base value (see `Validate`).
        let merged = match base {
            None => candidate,
            Some(base) => candidate.merge_over(base, &ExtractConfig::default()),
        };
        merged.validate()?;
        Ok(merged)
    }

    /// Option-B merge: fields whose candidate value matches `default` are "not
    /// defined" and inherit from `base`; everything else comes from the candidate.
    fn merge_over(&self, base: &Self, default: &Self) -> Self {
        Self {
            paths: self.paths.clone(),
            decompression: self.decompression,
            placement: self.placement.clone(),
            attributes: self.attributes.clone(),
            scan: merge_pick_clone(&self.scan, &base.scan, &default.scan),
            rehash: merge_pick_copy(self.rehash, base.rehash, default.rehash),
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
            owner_policy: merge_pick_clone(&self.owner_policy, &base.owner_policy, &default.owner_policy),
            owner_group: self.owner_group.clone(),
            mode_policy: merge_pick_clone(&self.mode_policy, &base.mode_policy, &default.mode_policy),
            strip_components: self.strip_components,
            transform_policy: merge_pick_clone(&self.transform_policy, &base.transform_policy, &default.transform_policy),
            filter: merge_pick_clone(&self.filter, &base.filter, &default.filter),
            sidecar: self.sidecar,
            hidden_sidecar: self.hidden_sidecar,
            bypass_archive: merge_pick_copy(self.bypass_archive, base.bypass_archive, default.bypass_archive),
        }
    }

    pub fn try_from(args: &ExtractArgs) -> Result<Self> {
        // INFO: c_* suffix in variables indicates that the paths were sanitized.
        let c_directory = resolve_cwd(args.directory.as_deref())?;
        let c_archive_path = resolve_path_to_abs_path(&args.archive, &c_directory);
        // Mode resolution: `--bypass-archive` extracts a directory product
        // (`-f DIR`), otherwise `-f` must be a tar archive file. No silent
        // inference — a `-f` pointing at the wrong kind is a hard error.
        if args.bypass_archive {
            if !c_archive_path.is_dir() {
                return Err(Error::Config(format!(
                    "`--bypass-archive` extracts a directory product, but {} is not a directory",
                    c_archive_path.display()
                )));
            }
        } else if !c_archive_path.is_file() {
            return Err(Error::Config(format!(
                "archive does not exist or is not a file: {}",
                c_archive_path.display()
            )));
        }

        let work_dir_location = args.work_dir_location;
        let c_output_dir = match &args.extract_to {
            Some(path) => resolve_path_to_abs_path(path, &c_directory),
            None => c_directory.clone(),
        };
        std::fs::create_dir_all(&c_output_dir).map_err(|e| Error::io(&c_output_dir, e))?;
        // In bypass mode the product dir IS the work dir: `db_path()` resolves to
        // `{product}/tar-dedup.sqlite` (the baked catalog) and `extract_cache_dir()`
        // to the product payloads, so the place/rehash/cleanup stages reuse them in
        // place.
        let c_work_dir = match &args.work_dir {
            Some(path) => {
                if args.bypass_archive {
                    return Err(Error::Config(String::from(
                        "--work-dir may not be passed with --bypass-archive"
                    )));
                }
                resolve_path_to_abs_path(path, &c_directory)
            },
            None if args.bypass_archive => c_archive_path.clone(),
            None => default_extract_work_dir(&c_archive_path, &c_output_dir, work_dir_location),
        };
        std::fs::create_dir_all(&c_work_dir).map_err(|e| Error::io(&c_work_dir, e))?;

        let start_policy = StartPolicy::create_or_fresh(args.fresh);

        let decompression = resolve_compression(&args.compression, &c_archive_path)?;

        validate_metadata_pairs(args)?;

        // Extract default: apply nothing. `--apply-all-metadata` flips the whole
        // group on; per-bit `--x`/`--no-x` override on top (explicit wins).
        let apply_all = args.apply_metadata;

        let apply_owner = resolve_bool_flag(args.no_apply_stored_owner_map,
                                            args.apply_stored_owner_map,
                                            apply_all);
        let apply_group = resolve_bool_flag(args.no_apply_stored_group_map,
                                            args.apply_stored_group_map,
                                            apply_all);
        // `same_owner` is the inference exception: explicit flags win, then the
        // INCLUDE base infers from the effective uid, EXCLUDE base means no owner.
        let same_owner = if args.no_same_owner {
            false
        } else if args.restore_owner {
            true
        } else {
            let owner = infer_same_owner();
            let owner_str = if owner { "--same-owner" } else { "--no-same-owner" };
            tracing::info!("Inferred: {owner_str}");
            owner
        };
        let apply_mode = resolve_bool_flag(args.no_apply_mode,
                                           args.apply_mode,
                                           apply_all);
        let apply_transform = resolve_bool_flag(args.no_apply_transform,
                                                args.apply_transform,
                                                apply_all);

        // Stored vs CLI owner/group policy. Explicit --owner/--group/--map take precedence;
        // resolved apply_owner/apply_group fall back to the archive's recorded policy.
        let owner_policy = resolve_owner_policy_from_args(
            args, &c_directory, apply_owner, apply_group
        )?;

        let mode_policy = resolve_mode_policy_from_args(args, apply_mode)?;
        
        let (apply_owner, apply_group) = if matches!(owner_policy, OwnerGroupSource::Cli(_)) {
            // INFO: Policy Decision - only 
            (false, false)
        } else {
            (apply_owner, apply_group)
        };
        
        let jobs = args.jobs.unwrap_or_else(num_cpus::get);
        let io_jobs = args.io_jobs.unwrap_or_else(num_cpus::get);

        let c_one_top_level = match &args.one_top_level {
            None => None,
            // Bare `--one-top-level`: default to the archive stem (GNU tar).
            Some(None) => Some(archive_stem(&c_archive_path).into()),
            Some(Some(otl)) => Some(otl.clean()),
        };

        let c_link_source = match args.link_source.as_ref() {
            None => None,
            Some(ls) => Some(ls.clean())
        };

        let sidecar_mode = if args.no_sidecar {
            if args.sidecar != SidecarMode::OnError {
                tracing::warn!(
                    "--no-sidecar is an alias for `--sidecar never`; explicit --sidecar ignored"
                );
            }
            SidecarMode::Never
        } else {
            args.sidecar
        };

        let keep_stage = if args.bypass_archive {
            if args.keep_stage != StageKeep::OnOther {
                tracing::warn!(
                    "With --bypass-archive, stage must be kept because it is the source directory");
            }
            StageKeep::Always
        } else {
            args.keep_stage
        };
        let db_retention = if args.bypass_archive {
            if args.db_retention != DbRetention::OnError {
                tracing::warn!(
                    "With --bypass-archive, db must be kept it is the source db");
            }
            DbRetention::Always
        } else {
            args.db_retention
        };


        Ok(Self {
            paths: PathLayout {
                archive_path: c_archive_path,
                directory: c_directory.clone(),
                output_dir: c_output_dir,
                work_dir: c_work_dir,
            },
            decompression,
            placement: PlacementOptions {
                absolute_names: args.absolute_names,
                one_top_level: c_one_top_level,
                keep_dir_symlink: args.keep_dir_symlink,
                unlink_first: args.unlink_first,
                no_create_dir: args.no_create_dir,
                conflict_policy: args.conflict_policy,
                silent_conflicts: args.silent_conflicts,
                remove_and_replace: args.remove_and_replace,
                link_tree: args.link_tree,
                use_hard_links: args.use_hard_links,
                absolute_links: args.absolute_links,
                clean_target: args.clean_target,
                link_source: c_link_source,
                no_reflink: args.no_reflink,
                hard_link_grouping: args.hard_link_grouping,
                recreate_none_file_entries: args.create_other_entries,
                lazy_delete_stage: args.lazy_delete_stage,
                strip_common_prefix: args.strip_common_prefix,
            },
            attributes: ExtractAttributeOptions {
                restore_owner: same_owner,
                no_overwrite_dir: args.no_overwrite_dir,
                overwrite_all_dir: args.overwrite_all_dir,
                apply_atime: resolve_bool_flag(args.no_apply_atime, args.apply_atime, apply_all),
                apply_mtime: resolve_bool_flag(args.no_apply_mtime, args.apply_mtime, apply_all),
                no_xattrs: !resolve_bool_flag(args.no_xattrs, args.xattrs, apply_all),
                no_acls: !resolve_bool_flag(args.no_acls, args.acls, apply_all),
                no_selinux: !resolve_bool_flag(args.no_selinux, args.selinux, apply_all),
                no_same_permissions: !resolve_bool_flag(args.no_same_permissions,
                                                        args.same_permissions,
                                                        apply_all),
            },
            scan: ScanOptions {
                sparsify: !args.no_sparsify,
                page_size: args.page_size,
                verify_archive: !args.no_verify_archive,
            },
            rehash: !args.no_rehash,
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
                force: args.force,
                relax_utf8: args.relax_utf8,
                progress_mode: args.progress_mode,
                cleanup: CleanupSettings {
                    db_retention,
                    keep_stage,
                },
                exit_after_stage: args.exit_after_stage.map(ExitAfterStage::from),
            },
            owner_policy,
            owner_group: OwnerGroupOptions {
                target: args.map_target,
                validate_maps: args.validate_maps,
                same_owner,
                apply_owner,
                apply_group,
            },
            mode_policy,
            strip_components: args.strip_components,
            transform_policy: resolve_transform_policy_from_args(args, apply_transform)?,
            filter: FilterOptions::build(&args.filter, &c_directory, !args.lazy_filter),
            sidecar: sidecar_mode,
            hidden_sidecar: args.hidden_sidecar,
            bypass_archive: args.bypass_archive,
        })
    }

    /// The sidecar mode actually in effect. `OnError` degrades to `Never` when
    /// `--no-errors` (nothing can be recorded) or `--link-tree` (no per-link
    /// permission errors exist; link-tree never offers `OnError`).
    pub fn effective_sidecar_mode(&self) -> SidecarMode {
        if self.sidecar == SidecarMode::OnError
            && (self.process.no_errors || self.placement.link_tree) {
            SidecarMode::Never
        } else {
            self.sidecar
        }
    }

    #[cfg(test)]
    pub fn for_place_test(extraction_root: PathBuf, absolute_names: bool, _jobs: usize) -> Self {
        Self {
            paths: PathLayout {
                archive_path: PathBuf::new(),
                directory: extraction_root.clone(),
                output_dir: extraction_root,
                work_dir: PathBuf::new(),
            },
            decompression: CompressionFormat::None,
            placement: PlacementOptions {
                absolute_names,
                one_top_level: None,
                keep_dir_symlink: false,
                unlink_first: false,
                no_create_dir: true,
                conflict_policy: ConflictPolicy::Replace,
                silent_conflicts: false,
                remove_and_replace: false,
                link_tree: false,
                use_hard_links: false,
                absolute_links: false,
                clean_target: false,
                link_source: None,
                no_reflink: false,
                hard_link_grouping: HardLinkGrouping::Global,
                recreate_none_file_entries: true,
                lazy_delete_stage: false,
                strip_common_prefix: false,
            },
            attributes: ExtractAttributeOptions::default(),
            scan: ScanOptions::default(),
            rehash: true,
            process: ProcessOptions::default(),
            owner_policy: OwnerGroupSource::None,
            owner_group: OwnerGroupOptions::default(),
            mode_policy: ModeSource::None,
            strip_components: 0,
            transform_policy: TransformSource::None,
            filter: FilterOptions::default(),
            sidecar: SidecarMode::Never,
            hidden_sidecar: false,
            bypass_archive: false,
        }
    }

    #[cfg(test)]
    pub fn for_scan_test(archive_path: PathBuf, work_dir: PathBuf, directory: PathBuf) -> Self {
        Self {
            paths: PathLayout {
                archive_path,
                directory: directory.clone(),
                output_dir: directory,
                work_dir,
            },
            decompression: CompressionFormat::None,
            placement: PlacementOptions {
                absolute_names: false,
                one_top_level: None,
                keep_dir_symlink: false,
                unlink_first: false,
                no_create_dir: false,
                conflict_policy: ConflictPolicy::Replace,
                silent_conflicts: false,
                remove_and_replace: false,
                link_tree: false,
                use_hard_links: false,
                absolute_links: false,
                clean_target: true,
                link_source: None,
                no_reflink: false,
                hard_link_grouping: HardLinkGrouping::Global,
                recreate_none_file_entries: true,
                lazy_delete_stage: false,
                strip_common_prefix: false,
            },
            attributes: ExtractAttributeOptions::default(),
            scan: ScanOptions::default(),
            rehash: true,
            process: ProcessOptions { jobs: 1, io_jobs: 1, force: true, ..ProcessOptions::default() },
            owner_policy: OwnerGroupSource::None,
            owner_group: OwnerGroupOptions::default(),
            mode_policy: ModeSource::None,
            strip_components: 0,
            transform_policy: TransformSource::None,
            filter: FilterOptions::default(),
            sidecar: SidecarMode::Never,
            hidden_sidecar: false,
            bypass_archive: false,
        }
    }

    /// Compute effective extraction root. (if given, concat one_top_level)
    pub fn effective_extraction_root(&self) -> PathBuf {
        match &self.placement.one_top_level {
            Some(otl) => self.paths.extraction_root().join(otl),
            None => self.paths.extraction_root().to_path_buf(),
        }
    }
}

// -------------------------------------------------------------------------------------------------
// Validation Helper Functions
// -------------------------------------------------------------------------------------------------

fn validate_name(name: &Path, source: &str) -> Result<()>{
    debug_assert_eq!(*name, name.clean(),
                     "PRECONDITION ERROR: All paths are expected to be clean: {source}");
    if name.components().count() != 1 {
        return Err(Error::Config(format!(
            "{source} must be a name, not a path."
        )));
    }
    if *name == PathBuf::from("") {
        return Err(Error::Config(format!(
            "{source} may not be empty."
        )));
    }
    Ok(())
}


/// Reject contradictory pairs like `--apply-atime --no-apply-atime` before building.
fn validate_metadata_pairs(args: &ExtractArgs) -> Result<()> {
    let pairs = [
        (args.restore_owner, args.no_same_owner, "--same-owner", "--no-same-owner"),
        (args.apply_stored_owner_map, args.no_apply_stored_owner_map, "--apply-stored-owner-map", "--no-apply-stored-owner-map"),
        (args.apply_stored_group_map, args.no_apply_stored_group_map, "--apply-stored-group-map", "--no-apply-stored-group-map"),
        (args.apply_mode, args.no_apply_mode, "--apply-mode", "--no-apply-mode"),
        (args.apply_transform, args.no_apply_transform, "--apply-transform", "--no-apply-transform"),
        (args.apply_atime, args.no_apply_atime, "--apply-atime", "--no-apply-atime"),
        (args.apply_mtime, args.no_apply_mtime, "--apply-mtime", "--no-apply-mtime"),
        (args.xattrs, args.no_xattrs, "--xattrs", "--no-xattrs"),
        (args.acls, args.no_acls, "--acls", "--no-acls"),
        (args.selinux, args.no_selinux, "--selinux", "--no-selinux"),
        (args.same_permissions, args.no_same_permissions, "--same-permissions", "--no-same-permissions"),
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

// -------------------------------------------------------------------------------------------------
// Resolve Helper Functions
// -------------------------------------------------------------------------------------------------

/// Resolve which owner/group policy applies on extract from the CLI args.
///
/// Identity is all-or-nothing (per `--owner`/`--apply-stored-*` docs): explicit
/// `--owner`/`--group`/`--owner-map`/`--group-map` map to a single CLI policy for
/// **both** identities and take precedence over the apply bits, which then fall
/// back to the archive's recorded policy only when **none** of the four is given
/// (`Stored`). No mixing of one identity from the CLI and the other from the
/// archive.
fn resolve_owner_policy_from_args(
    args: &ExtractArgs,
    directory: &Path,
    apply_owner: bool,
    apply_group: bool)
    -> Result<OwnerGroupSource> {
    let has_cli = args.owner.is_some()
        || args.group.is_some()
        || args.owner_map.is_some()
        || args.group_map.is_some();

    if has_cli {
        if apply_owner || apply_group {
            tracing::warn!(
                "--apply-stored-owner-map / --apply-stored-group-map ignored; \
                 explicit --owner/--group/--owner-map/--group-map take precedence"
            );
        }
        let owner_map = resolve_map_arg(
            args.owner_map.as_ref(),"--owner-map", directory
        )?;
        let group_map = resolve_map_arg(
            args.group_map.as_ref(),"--group-map", directory
        )?;
        match parse_owner_group_args(
            args.owner.as_deref(),
            owner_map.as_deref(),
            args.group.as_deref(),
            group_map.as_deref(),
        )? {
            Some(policy) => {
                if args.validate_maps {
                    validate_for_resolution_mode(&policy, args.map_target)?;
                }
                Ok(OwnerGroupSource::Cli(policy))
            }
            None => Ok(OwnerGroupSource::None),
        }
    } else if apply_owner || apply_group {
        Ok(OwnerGroupSource::Stored)
    } else {
        Ok(OwnerGroupSource::None)
    }
}

/// Resolve which mode-change policy applies on extract from the CLI args.
/// Explicit `--mode` takes precedence over `apply_mode` (which falls back to
/// the archive's recorded changes).
fn resolve_mode_policy_from_args(args: &ExtractArgs, apply_mode: bool) -> Result<ModeSource> {
    if let Some(changes) = &args.mode {
        if apply_mode {
            tracing::warn!("--apply-mode ignored; explicit --mode takes precedence");
        }
        parse_mode_changes(changes)?;
        Ok(ModeSource::Cli(changes.clone()))
    } else if apply_mode {
        Ok(ModeSource::Stored)
    } else {
        Ok(ModeSource::None)
    }
}

/// Resolve which name-transform policy applies on extract from the CLI args.
/// Explicit `--transform` takes precedence over `apply_transform` (which
/// falls back to the archive's recorded expression).
fn resolve_transform_policy_from_args(args: &ExtractArgs, apply_transform: bool)
    -> Result<TransformSource> {
    if let Some(expr) = &args.transform {
        if apply_transform {
            tracing::warn!("--apply-transform ignored; explicit --transform takes precedence");
        }
        parse_transform_expr(expr)?;
        Ok(TransformSource::Cli(expr.clone()))
    } else if apply_transform {
        Ok(TransformSource::Stored)
    } else {
        Ok(TransformSource::None)
    }
}

/// Resolve a `--owner-map` / `--group-map` file path against `directory` and validate it.
fn resolve_map_arg(
    map: Option<&PathBuf>,
    label: &str,
    directory: &Path)
    -> Result<Option<PathBuf>> {
    match map {
        None => Ok(None),
        Some(p) => {
            let resolved = resolve_path_to_abs_path(p, directory);
            validate_file(&resolved, label)?;
            Ok(Some(resolved))
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// TEST
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::{Cli, Command};
    use crate::common::perms::infer_same_owner;
    use crate::config::{CompressionFormat, DbRetention, SidecarMode, WorkDirLocation};
    use clap::Parser;
    use std::path::{Path, PathBuf};

    /// `Cli::try_parse_from` on `extract -f <archive> [extra...]`.
    fn parse_extract(archive: &Path, extra: &[&str]) -> ExtractArgs {
        let argv = std::iter::once("tar-dedup".to_string())
            .chain(std::iter::once("extract".to_string()))
            .chain(["-f".to_string(), archive.display().to_string()])
            .chain(extra.iter().map(|s| s.to_string()))
            .collect::<Vec<_>>();
        let cli = Cli::try_parse_from(&argv).expect("clap parse");
        match cli.command {
            Command::Extract(a) => a,
            other => panic!("expected extract, got {other:?}"),
        }
    }

    fn archive_file(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, b"dummy").expect("write archive fixture");
        p
    }

    fn build(archive: &Path, extra: &[&str]) -> Result<ExtractConfig> {
        ExtractConfig::build(&parse_extract(archive, extra), None)
    }

    /// The whole contradictory `--x` / `--no-x` family in ONE matrix test (not
    /// one test per pair): each pair on one command line is rejected, each flag
    /// alone is accepted.
    #[test]
    fn conflicting_metadata_pairs_rejected_in_one_matrix() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(dir.path(), "backup.tar");
        let pairs: [(&str, &str); 11] = [
            ("--same-owner", "--no-same-owner"),
            ("--apply-stored-owner-map", "--no-apply-stored-owner-map"),
            ("--apply-stored-group-map", "--no-apply-stored-group-map"),
            ("--apply-mode", "--no-apply-mode"),
            ("--apply-transform", "--no-apply-transform"),
            ("--apply-atime", "--no-apply-atime"),
            ("--apply-mtime", "--no-apply-mtime"),
            ("--xattrs", "--no-xattrs"),
            ("--acls", "--no-acls"),
            ("--selinux", "--no-selinux"),
            ("--same-permissions", "--no-same-permissions"),
        ];
        for (yes, no) in pairs {
            let err = build(&archive, &[yes, no])
                .expect_err(&format!("{yes} + {no} must be rejected"));
            assert!(
                err.to_string().contains("conflicting flags"),
                "{yes}+{no}: {err}"
            );
            assert!(err.to_string().contains(yes), "{yes}+{no}: {err}");
            assert!(err.to_string().contains(no), "{yes}+{no}: {err}");
            // Only the pair conflicts; each flag alone is valid.
            assert!(build(&archive, &[yes]).is_ok(), "{yes} alone");
            assert!(build(&archive, &[no]).is_ok(), "{no} alone");
        }
    }

    #[test]
    fn missing_archive_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope.tar");
        let err = build(&missing, &[]).unwrap_err();
        assert!(
            err.to_string().contains("archive does not exist"),
            "missing archive: {err}"
        );
    }

    #[test]
    fn minimal_valid_config_absolutizes_and_stages_work_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(dir.path(), "backup.tar");
        let cfg = build(&archive, &[]).expect("build");
        assert_eq!(cfg.paths.archive_path, archive, "already absolute");
        let expected_work = crate::config::default_extract_work_dir(
            &archive,
            dir.path(),
            WorkDirLocation::BesideArchive,
        );
        assert_eq!(cfg.paths.work_dir, expected_work);
        assert!(cfg.rehash, "rehash on by default");
        assert!(cfg.scan.verify_archive, "archive verify on by default");
        assert_eq!(cfg.owner_group.same_owner, infer_same_owner());
        assert_eq!(cfg.strip_components, 0);
        assert_eq!(cfg.decompression, CompressionFormat::None);
    }

    #[test]
    fn one_top_level_uses_stem_or_explicit_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(dir.path(), "named.tar");
        let cfg = build(&archive, &["--one-top-level"]).expect("bare");
        assert_eq!(cfg.placement.one_top_level.as_deref(), Some(Path::new("named")));
        let cfg2 = build(&archive, &["--one-top-level", "itsy"]).expect("explicit");
        assert_eq!(cfg2.placement.one_top_level.as_deref(), Some(Path::new("itsy")));
    }

    #[test]
    fn clean_target_conflicts_with_no_create_dirs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(dir.path(), "backup.tar");
        let err = build(&archive, &["--clean-target", "--no-create-dirs"]).unwrap_err();
        assert!(
            err.to_string().contains("--clean-target conflicts with --no-create-dirs"),
            "{err}"
        );
    }

    #[test]
    fn restore_owner_and_no_same_owner_set_the_same_owner_bit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(dir.path(), "backup.tar");
        let yes = build(&archive, &["--restore-owner"]).expect("restore-owner");
        assert!(yes.owner_group.same_owner);
        assert!(yes.attributes.restore_owner);
        let no = build(&archive, &["--no-same-owner"]).expect("no-same-owner");
        assert!(!no.owner_group.same_owner);
    }

    #[test]
    fn apply_all_metadata_overridden_per_bit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(dir.path(), "backup.tar");
        let cfg = build(&archive, &["--apply-all-metadata", "--no-apply-atime"])
            .expect("build");
        assert!(!cfg.attributes.apply_atime, "--no-apply-atime wins");
        assert!(cfg.attributes.apply_mtime);
        assert!(!cfg.attributes.no_xattrs, "umbrella on xattrs");
        assert!(!cfg.attributes.no_acls, "umbrella on acls");
        // The umbrella sets the stored owner/group-map apply bits (owner
        // identity itself stays the inference path + explicit --same-owner).
        assert!(cfg.owner_group.apply_owner);
        assert!(cfg.owner_group.apply_group);

        let base = build(&archive, &[]).expect("build");
        assert!(!base.owner_group.apply_owner);
        assert!(!base.owner_group.apply_group);
    }

    #[test]
    fn sparsify_requires_nonzero_page_size() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(dir.path(), "backup.tar");
        let err = build(&archive, &["--sparsify", "--page-size", "0"]).unwrap_err();
        assert!(
            err.to_string().contains("--page-size"),
            "zero page size: {err}"
        );
    }

    #[test]
    fn invalid_mode_and_transform_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(dir.path(), "backup.tar");
        let err = build(&archive, &["--mode", "nonsense"]).unwrap_err();
        assert!(!err.to_string().is_empty(), "--mode parse must fail");
        let err2 = build(&archive, &["--transform", "nonsense"]).unwrap_err();
        assert!(!err2.to_string().is_empty(), "--transform parse must fail");
    }

    #[test]
    fn zero_worker_counts_are_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(dir.path(), "backup.tar");
        for flag in ["--jobs", "--io-jobs"] {
            let err = build(&archive, &[flag, "0"]).unwrap_err();
            assert!(
                err.to_string().contains("must be greater than 0"),
                "{flag}: {err}"
            );
        }
        let cfg = build(&archive, &["--jobs", "2"]).expect("jobs 2");
        assert_eq!(cfg.process.jobs, 2);
    }

    #[test]
    fn compression_inferred_from_suffix_and_overridden() {
        let dir = tempfile::tempdir().expect("tempdir");
        let xz = archive_file(dir.path(), "backup.tdd.tar.xz");
        let cfg = build(&xz, &[]).expect("suffix inference");
        assert_eq!(cfg.decompression, CompressionFormat::Xz);

        let plain = archive_file(dir.path(), "plain");
        let gz = build(&plain, &["-z"]).expect("force gzip");
        assert_eq!(gz.decompression, CompressionFormat::Gz);
        let xz2 = build(&plain, &["-J"]).expect("force xz");
        assert_eq!(xz2.decompression, CompressionFormat::Xz);
    }

    #[test]
    fn conflicting_compression_flags_fail_at_parse() {
        let argv = ["tar-dedup", "extract", "-f", "x.tar", "-z", "-J"];
        assert!(Cli::try_parse_from(argv).is_err(), "-z + -J must conflict");
    }

    #[test]
    fn work_dir_location_beside_output_moves_work_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(dir.path(), "backup.tar");
        let cfg = build(&archive, &["--work-dir-location", "beside-output"]).expect("build");
        let expected = crate::config::default_extract_work_dir(
            &archive,
            &cfg.paths.output_dir,
            WorkDirLocation::BesideOutput,
        );
        assert_eq!(cfg.paths.work_dir, expected);
    }

    #[test]
    fn sidecar_flags_build_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(dir.path(), "backup.tar");

        let cfg = build(&archive, &[]).expect("default");
        assert_eq!(cfg.sidecar, SidecarMode::OnError);
        assert!(!cfg.hidden_sidecar);

        let cfg = build(&archive, &["--sidecar", "always", "--hidden-sidecar"]).expect("always");
        assert_eq!(cfg.sidecar, SidecarMode::Always);
        assert!(cfg.hidden_sidecar);

        for args in [&["--no-sidecar"][..], &["--sidecar", "never"][..]] {
            let cfg = build(&archive, args).expect("never");
            assert_eq!(cfg.sidecar, SidecarMode::Never);
        }
    }

    #[test]
    fn bypass_archive_mode_resolves_work_dir_to_product_and_validates() {
        let dir = tempfile::tempdir().expect("tempdir");
        // `-f DIR` + `--bypass-archive`: the product dir becomes the work dir.
        let product = dir.path().join("product");
        std::fs::create_dir_all(&product).expect("mk product");
        let cfg = build(&product, &["--bypass-archive"]).expect("build bypass");
        assert!(cfg.bypass_archive, "bypass flag set");
        assert_eq!(cfg.paths.work_dir, product, "product dir doubles as work dir");
        assert_eq!(cfg.paths.db_path(), product.join("tar-dedup.sqlite"));

        // `-f DIR` without the flag is a hard error (not a tar file).
        let err = build(&product, &[]).unwrap_err();
        assert!(err.to_string().contains("not a file"), "{err}");

        // `-f <file>` + `--bypass-archive` is a hard error (product must be a dir).
        let arc = archive_file(dir.path(), "x.tar");
        let err = build(&arc, &["--bypass-archive"]).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err}");
        // `-f <file>` without the flag remains valid (normal tar extract).
        assert!(build(&arc, &[]).is_ok());
    }

    #[test]
    fn no_rehash_with_bypass_requires_force() {
        let dir = tempfile::tempdir().expect("tempdir");
        let product = dir.path().join("product");
        std::fs::create_dir_all(&product).expect("mk product");
        // `--no-rehash` on a bypass product is rejected unless --force.
        let err = build(&product, &["--bypass-archive", "--no-rehash"]).unwrap_err();
        assert!(
            err.to_string().contains("--no-rehash") && err.to_string().contains("--force"),
            "{err}"
        );
        // With --force it is accepted.
        assert!(build(&product, &["--bypass-archive", "--no-rehash", "--force"]).is_ok());
    }

    #[test]
    fn db_retention_flag_builds_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(dir.path(), "backup.tar");

        let cfg = build(&archive, &[]).expect("default");
        assert_eq!(cfg.process.cleanup.db_retention, DbRetention::OnError);

        for (flag, expect) in [("always", DbRetention::Always), ("never", DbRetention::Never)] {
            let cfg = build(&archive, &["--db-retention", flag]).expect(flag);
            assert_eq!(cfg.process.cleanup.db_retention, expect, "--db-retention {flag}");
        }
    }

    #[test]
    fn effective_sidecar_mode_folds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(dir.path(), "backup.tar");
        let mut cfg = build(&archive, &["--sidecar", "on-error"]).expect("on-error");
        assert_eq!(cfg.effective_sidecar_mode(), SidecarMode::OnError);

        cfg.process.no_errors = true;
        assert_eq!(
            cfg.effective_sidecar_mode(),
            SidecarMode::Never,
            "--no-errors folds on-error sidecars to never"
        );

        let mut cfg2 = build(&archive, &["--sidecar", "on-error"]).expect("on-error");
        cfg2.placement.link_tree = true;
        assert_eq!(
            cfg2.effective_sidecar_mode(),
            SidecarMode::Never,
            "link-tree folds on-error sidecars to never"
        );
    }

    #[test]
    fn strip_common_prefix_flag_builds_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let archive = archive_file(dir.path(), "backup.tar");
        let cfg = build(&archive, &["--strip-common-prefix"]).expect("flag");
        assert!(cfg.placement.strip_common_prefix, "--strip-common-prefix maps into placement");
        let cfg = build(&archive, &[]).expect("default");
        assert!(!cfg.placement.strip_common_prefix, "defaults to off");
    }
}