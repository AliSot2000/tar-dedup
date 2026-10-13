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
use super::{
    default_extract_work_dir, resolve_cwd, resolve_path_to_abs_path, validate_file,
};

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
        if self.link_source.is_some() && !self.link_tree {
            tracing::warn!(
                "--link-source requires --link-tree"
            );
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
    pub force_overwrite_dir: bool,
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
            force_overwrite_dir: false,
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
        if self.no_overwrite_dir && self.force_overwrite_dir {
            tracing::warn!("--force-overwrite-dir is disabled with --no-overwrite-dir");
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
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            sparsify: false,
            page_size: 4096,
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
        }
    }

    pub fn try_from(args: &ExtractArgs) -> Result<Self> {
        // INFO: c_* suffix in variables indicates that the paths were sanitized.
        let c_directory = resolve_cwd(args.directory.as_deref())?;
        let c_archive_path = resolve_path_to_abs_path(&args.archive, &c_directory);
        if !c_archive_path.is_file() {
            return Err(Error::Config(format!(
                "archive does not exist or is not a file: {}",
                c_archive_path.display()
            )));
        }
        
        let extract_stage_location = args.stage_location;
        let c_output_dir = match &args.extract_to {
            Some(path) => resolve_path_to_abs_path(path, &c_directory),
            None => c_directory.clone(),
        };
        std::fs::create_dir_all(&c_output_dir).map_err(|e| Error::io(&c_output_dir, e))?;
        let c_work_dir = match &args.work_dir {
            Some(path) => resolve_path_to_abs_path(path, &c_directory),
            None => default_extract_work_dir(&c_archive_path, &c_output_dir, extract_stage_location),
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

        let c_one_top_level = match args.one_top_level.as_ref() {
            None => None,
            Some(otl) => Some(otl.clean())
        };

        let c_link_source = match args.link_source.as_ref() {
            None => None,
            Some(ls) => Some(ls.clean())
        };

        Ok(Self {
            paths: PathLayout {
                archive_path: c_archive_path,
                directory: c_directory,
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
            },
            attributes: ExtractAttributeOptions {
                restore_owner: same_owner,
                no_overwrite_dir: args.no_overwrite_dir,
                force_overwrite_dir: args.force_overwrite_dir,
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
                sparsify: args.sparsify,
                page_size: args.page_size,
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
                cleanup: CleanupSettings::from_flags(args.keep_db, args.keep_stage),
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
            filter: FilterOptions {
                exclude_patterns: args.exclude.clone(),
                include_patterns: args.include.clone(),
                exclude_from: args.exclude_from.clone(),
                include_from: args.include_from.clone(),
                anchored: args.anchored,
                ignore_case: args.ignore_case,
                no_unicode: args.no_unicode,
                eager_filter: !args.lazy_filter,
            },
        })
    }

    #[cfg(test)]
    pub fn for_place_test(extraction_root: PathBuf, absolute_names: bool, jobs: usize) -> Self {
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