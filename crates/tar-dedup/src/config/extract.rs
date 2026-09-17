use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cli::{ConflictPolicy, ExtractArgs, HardLinkGrouping};
use crate::common::perms::{
    MapResolutionTarget, ModeSource, OwnerGroupSource, infer_same_owner, parse_mode_changes,
    parse_owner_group_args, validate_for_mode,
};
use crate::common::start::StartPolicy;
use crate::common::transform::{TransformSource, parse_transform_expr};
use crate::error::{Error, Result};

use super::compression::infer_compression_from_suffix;
use super::paths::PathLayout;
use super::process::{CleanupSettings, ProcessOptions};
use super::{
    ExtractStageLocation, default_extract_work_dir, resolve_cwd, resolve_path_to_abs_path,
    validate_file,
};
use crate::config::FilterOptions;

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
    pub absolute_links: bool, // TODO not possible with use_hard_links
    pub clean_target: bool,   // TODO implies no_create_dir is false!
    pub link_source: Option<PathBuf>, // TODO CLI + san!!! (may be file name only), need not exist when starting extraction
    pub no_reflink: bool,             // TODO cli
    pub hard_link_grouping: HardLinkGrouping, // TODO cli
    pub recreate_none_file_entries: bool, // TODO CLI
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerGroupOptions {
    pub target: MapResolutionTarget,
    pub validate_maps: bool,
    pub same_owner: bool,
    pub apply_owner: bool,
    pub apply_group: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanOptions {
    pub force_scan: bool,
    pub rehash: bool,
    pub clear_archive_meta: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractConfig {
    pub force: bool, // Will force the system just try anyway even tho stuff failed. Might lead to inconsistent states and corrupted data.
    pub paths: PathLayout,
    pub decompression: super::compression::CompressionFormat,
    pub placement: PlacementOptions,
    pub attributes: ExtractAttributeOptions,
    pub scan: ScanOptions,
    pub process: ProcessOptions,
    pub owner_policy: OwnerGroupSource,
    pub owner_group: OwnerGroupOptions,
    pub mode_policy: ModeSource,
    pub strip_components: u32,
    pub transform_policy: TransformSource,
    pub filter: FilterOptions,
}

/// Resolve which owner/group policy applies on extract from the CLI args.
///
/// Explicit `--owner`/`--group`/`--owner-map`/`--group-map` map to a CLI policy and
/// take precedence over `--apply-stored-*`. Otherwise the stored policy is requested
/// (`Stored`), to be fetched from the archive footprint by the caller.
fn resolve_owner_policy_from_args(
    args: &ExtractArgs,
    directory: &Path,
) -> Result<OwnerGroupSource> {
    let has_cli = args.owner.is_some()
        || args.group.is_some()
        || args.owner_map.is_some()
        || args.group_map.is_some();

    if has_cli {
        if args.apply_stored_owner_map || args.apply_stored_group_map {
            tracing::warn!(
                "--apply-stored-owner-map / --apply-stored-group-map ignored; \
                 explicit --owner/--group/--owner-map/--group-map take precedence"
            );
        }
        let owner_map = resolve_map_arg(args.owner_map.as_ref(), "--owner-map", directory)?;
        let group_map = resolve_map_arg(args.group_map.as_ref(), "--group-map", directory)?;
        match parse_owner_group_args(
            args.owner.as_deref(),
            owner_map.as_deref(),
            args.group.as_deref(),
            group_map.as_deref(),
        )? {
            Some(policy) => {
                if args.validate_maps {
                    validate_for_mode(&policy, args.map_target)?;
                }
                Ok(OwnerGroupSource::Cli(policy))
            }
            None => Ok(OwnerGroupSource::None),
        }
    } else if args.apply_stored_owner_map || args.apply_stored_group_map || args.apply_metadata {
        Ok(OwnerGroupSource::Stored)
    } else {
        Ok(OwnerGroupSource::None)
    }
}

/// Resolve which mode-change policy applies on extract from the CLI args.
/// Explicit `--mode` takes precedence over `--apply-mode` (which falls back to
/// the archive's recorded changes).
fn resolve_mode_policy_from_args(args: &ExtractArgs) -> Result<ModeSource> {
    if let Some(changes) = &args.mode {
        if args.apply_mode {
            tracing::warn!("--apply-mode ignored; explicit --mode takes precedence");
        }
        parse_mode_changes(changes)?;
        Ok(ModeSource::Cli(changes.clone()))
    } else if args.apply_mode || args.apply_metadata {
        Ok(ModeSource::Stored)
    } else {
        Ok(ModeSource::None)
    }
}

/// Resolve which name-transform policy applies on extract from the CLI args.
/// Explicit `--transform` takes precedence over `--apply-transform` (which
/// falls back to the archive's recorded expression).
fn resolve_transform_policy_from_args(args: &ExtractArgs) -> Result<TransformSource> {
    if let Some(expr) = &args.transform {
        if args.apply_transform {
            tracing::warn!("--apply-transform ignored; explicit --transform takes precedence");
        }
        parse_transform_expr(expr)?;
        Ok(TransformSource::Cli(expr.clone()))
    } else if args.apply_transform || args.apply_metadata {
        Ok(TransformSource::Stored)
    } else {
        Ok(TransformSource::None)
    }
}

/// Resolve a `--owner-map` / `--group-map` file path against `directory` and validate it.
fn resolve_map_arg(
    map: Option<&PathBuf>,
    label: &str,
    directory: &Path,
) -> Result<Option<PathBuf>> {
    match map {
        None => Ok(None),
        Some(p) => {
            let resolved = resolve_path_to_abs_path(p, directory);
            validate_file(&resolved, label)?;
            Ok(Some(resolved))
        }
    }
}

impl ExtractConfig {
    /// Build from CLI args, optionally inheriting `base` where the args do not
    /// define a value (Option-B merge: a field whose arg-derived value matches the
    /// default is "not defined" and inherits from `base`).
    pub fn build(args: &ExtractArgs, base: Option<&ExtractConfig>) -> Result<Self> {
        let candidate = Self::try_from(args)?;
        match base {
            None => Ok(candidate),
            Some(base) => Ok(candidate.merge_over(base, &DEFAULT_CONFIG)),
        }
    }

    pub fn try_from(args: &ExtractArgs) -> Result<Self> {
        let directory = resolve_cwd(args.directory.as_deref())?;
        let archive_path = resolve_path_to_abs_path(&args.archive, &directory);
        if !archive_path.is_file() {
            return Err(Error::Config(format!(
                "archive does not exist or is not a file: {}",
                archive_path.display()
            )));
        }

        std::fs::create_dir_all(&directory).map_err(|e| Error::io(&directory, e))?;

        let extract_stage_location = ExtractStageLocation::BesideArchive;
        let work_dir = match &args.work_dir {
            Some(path) => resolve_path_to_abs_path(path, &directory),
            None => default_extract_work_dir(&archive_path, &directory, extract_stage_location),
        };
        std::fs::create_dir_all(&work_dir).map_err(|e| Error::io(&work_dir, e))?;

        let decompression = infer_compression_from_suffix(&archive_path);
        let start_policy = StartPolicy::create_or_fresh(args.fresh);

        // Emit warning
        if args.absolute_names && matches!(args.hard_link_grouping, HardLinkGrouping::Source) {
            tracing::warn!(
                "Materializing with --absolute-names and per source hardlink recreation might cause \
                disjoint subgroups of hardlinks that formerly were a single hardlink.");
        }

        // TODO clean_target and one_top_level IS NONE => warning, no effect.

        if args.no_same_owner && args.restore_owner {
            tracing::warn!(
                "Got --no-same-owner and --same-owner or --restore-owner. Option is inferred \
                from process uid and both flags are ignored."
            );
        }

        // Stored vs CLI owner/group policy. Explicit --owner/--group/--map take precedence;
        // --apply-stored-* fall back to the archive's recorded policy.
        let owner_policy = resolve_owner_policy_from_args(args, &directory)?;

        let mode_policy = resolve_mode_policy_from_args(args)?;

        let (apply_owner, apply_group) = if matches!(owner_policy, OwnerGroupSource::Cli(_)) {
            (false, false)
        } else {
            (args.apply_stored_owner_map, args.apply_stored_group_map)
        };
        let jobs = args.jobs.unwrap_or_else(num_cpus::get);
        let io_jobs = args.io_jobs.unwrap_or_else(num_cpus::get);
        Ok(Self {
            force: true, // TODO cli
            paths: PathLayout {
                archive_path,
                directory,
                work_dir,
            },
            decompression,
            placement: PlacementOptions {
                absolute_names: args.absolute_names,
                one_top_level: args.one_top_level.clone(),
                keep_dir_symlink: args.keep_dir_symlink,
                unlink_first: args.unlink_first,
                no_create_dir: args.no_create_dir,
                conflict_policy: args.conflict_policy,
                silent_conflicts: args.silent_conflicts,
                remove_and_replace: args.remove_and_replace,
                link_tree: args.link_tree,
                use_hard_links: args.use_hard_links,
                absolute_links: args.absolute_links,
                clean_target: true,
                link_source: None,
                no_reflink: false,
                hard_link_grouping: HardLinkGrouping::Global,
                recreate_none_file_entries: true,
            },
            attributes: ExtractAttributeOptions {
                restore_owner: if args.apply_metadata {
                    !args.no_same_owner
                } else {
                    args.restore_owner
                },
                no_overwrite_dir: args.no_overwrite_dir,
                force_overwrite_dir: args.force_overwrite_dir,
                apply_atime: if args.apply_metadata {
                    !args.no_apply_atime
                } else {
                    args.apply_atime
                },
                apply_mtime: if args.apply_metadata {
                    !args.no_apply_mtime
                } else {
                    args.apply_mtime
                },
                no_xattrs: if args.apply_metadata && !args.no_xattrs {
                    false
                } else {
                    args.no_xattrs
                },
                no_acls: if args.apply_metadata && !args.no_acls {
                    false
                } else {
                    args.no_acls
                },
                no_selinux: if args.apply_metadata && !args.no_selinux {
                    false
                } else {
                    args.no_selinux
                },
                no_same_permissions: if args.apply_metadata && !args.no_same_permissions {
                    false
                } else {
                    args.no_same_permissions
                },
            },
            scan: ScanOptions {
                force_scan: false,
                rehash: true,
                clear_archive_meta: false,
            },
            process: ProcessOptions {
                start_policy,
                jobs,
                io_jobs,
                fail_fast: args.fail_fast,
                no_errors: args.no_errors,
                cleanup: CleanupSettings::from_flags(args.keep_db, args.keep_stage),
                exit_after_stage: None,
            },
            owner_policy,
            owner_group: OwnerGroupOptions {
                target: args.map_target,
                validate_maps: args.validate_maps,
                same_owner: if args.no_same_owner {
                    false
                } else if args.restore_owner {
                    true
                } else {
                    let inferred_owner = infer_same_owner();
                    let argument = if inferred_owner {
                        "--same-owner"
                    } else {
                        "--no-same-owner"
                    };
                    tracing::info!("Inferred: {argument}");
                    inferred_owner
                },
                apply_owner,
                apply_group,
            },
            mode_policy,
            strip_components: args.strip_components,
            transform_policy: resolve_transform_policy_from_args(args)?,
            filter: FilterOptions {
                exclude_patterns: args.exclude.clone(),
                include_patterns: args.include.clone(),
                exclude_from: args.exclude_from.clone(),
                include_from: args.include_from.clone(),
                anchored: args.anchored,
                ignore_case: args.ignore_case,
                eager_filter: !args.lazy_filter,
            },
        })
    }

    #[cfg(test)]
    pub fn for_place_test(extraction_root: PathBuf, absolute_names: bool, jobs: usize) -> Self {
        Self {
            force: true,
            paths: PathLayout {
                archive_path: PathBuf::new(),
                directory: extraction_root,
                work_dir: PathBuf::new(),
            },
            decompression: super::compression::CompressionFormat::None,
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
            attributes: ExtractAttributeOptions {
                restore_owner: false,
                no_overwrite_dir: false,
                force_overwrite_dir: false,
                apply_atime: false,
                apply_mtime: false,
                no_xattrs: false,
                no_acls: false,
                no_selinux: false,
                no_same_permissions: false,
            },
            scan: ScanOptions {
                force_scan: false,
                rehash: true,
                clear_archive_meta: false,
            },
            process: ProcessOptions {
                start_policy: StartPolicy::Create,
                jobs,
                io_jobs: jobs,
                fail_fast: false,
                no_errors: false,
                cleanup: CleanupSettings { keep_db: false, keep_stage: false },
                exit_after_stage: None,
            },
            owner_policy: OwnerGroupSource::None,
            owner_group: OwnerGroupOptions {
                target: MapResolutionTarget::NameId,
                validate_maps: false,
                same_owner: false,
                apply_owner: false,
                apply_group: false,
            },
            mode_policy: ModeSource::None,
            strip_components: 0,
            transform_policy: TransformSource::None,
            filter: FilterOptions {
                exclude_patterns: Vec::new(),
                include_patterns: Vec::new(),
                exclude_from: Vec::new(),
                include_from: Vec::new(),
                anchored: false,
                ignore_case: false,
                eager_filter: true,
            },
        }
    }

    #[cfg(test)]
    pub fn for_scan_test(archive_path: PathBuf, work_dir: PathBuf, directory: PathBuf) -> Self {
        Self {
            force: true, // TODO cli
            paths: PathLayout {
                archive_path,
                directory,
                work_dir,
            },
            decompression: super::compression::CompressionFormat::None,
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
                clean_target: true, // TODO cli
                link_source: None,
                no_reflink: false,
                hard_link_grouping: HardLinkGrouping::Global,
                recreate_none_file_entries: true,
            },
            attributes: ExtractAttributeOptions {
                restore_owner: false,
                no_overwrite_dir: false,
                force_overwrite_dir: false,
                apply_atime: false,
                apply_mtime: false,
                no_xattrs: false,
                no_acls: false,
                no_selinux: false,
                no_same_permissions: false,
            },
            scan: ScanOptions {
                force_scan: false,
                rehash: true,
                clear_archive_meta: false,
            },
            process: ProcessOptions {
                start_policy: StartPolicy::Create,
                jobs: 1,
                io_jobs: 1,
                fail_fast: false,
                no_errors: false,
                cleanup: CleanupSettings { keep_db: false, keep_stage: false },
                exit_after_stage: None,
            },
            owner_policy: OwnerGroupSource::None,
            owner_group: OwnerGroupOptions {
                target: MapResolutionTarget::NameId,
                validate_maps: false,
                same_owner: false,
                apply_owner: false,
                apply_group: false,
            },
            mode_policy: ModeSource::None,
            strip_components: 0,
            transform_policy: TransformSource::None,
            filter: FilterOptions {
                exclude_patterns: Vec::new(),
                include_patterns: Vec::new(),
                exclude_from: Vec::new(),
                include_from: Vec::new(),
                anchored: false,
                ignore_case: false,
                eager_filter: true,
            },
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

    fn kept_db_parent<'a>(&'a self, mode: super::CleanupMode) -> &'a Path {
        match mode {
            super::CleanupMode::Archive => super::path_parent(&self.paths.archive_path),
            super::CleanupMode::Extract => self.paths.extraction_root(),
        }
    }
}

impl ExtractConfig {
    /// Option-B merge: fields whose candidate value matches `default` are "not
    /// defined" and inherit from `base`; everything else comes from the candidate.
    fn merge_over(&self, base: &Self, default: &Self) -> Self {
        Self {
            force: self.force,
            paths: self.paths.clone(),
            decompression: self.decompression,
            placement: self.placement.clone(),
            attributes: self.attributes.clone(),
            scan: merge_pick(&self.scan, &base.scan, &default.scan),
            process: ProcessOptions {
                start_policy: self.process.start_policy,
                jobs: merge_pick_u(self.process.jobs, base.process.jobs, default.process.jobs),
                io_jobs: merge_pick_u(self.process.io_jobs, base.process.io_jobs, default.process.io_jobs),
                fail_fast: merge_pick_b(self.process.fail_fast, base.process.fail_fast, default.process.fail_fast),
                no_errors: merge_pick_b(self.process.no_errors, base.process.no_errors, default.process.no_errors),
                cleanup: self.process.cleanup,
                exit_after_stage: self.process.exit_after_stage,
            },
            owner_policy: merge_pick(&self.owner_policy, &base.owner_policy, &default.owner_policy),
            owner_group: self.owner_group.clone(),
            mode_policy: merge_pick(&self.mode_policy, &base.mode_policy, &default.mode_policy),
            strip_components: self.strip_components,
            transform_policy: merge_pick(&self.transform_policy, &base.transform_policy, &default.transform_policy),
            filter: merge_pick(&self.filter, &base.filter, &default.filter),
        }
    }
}

fn merge_pick<T: PartialEq + Clone>(cand: &T, base: &T, default: &T) -> T {
    if cand == default { base.clone() } else { cand.clone() }
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

fn merge_pick_u(cand: usize, base: usize, default: usize) -> usize {
    if cand == default { base } else { cand }
}

fn merge_pick_b(cand: bool, base: bool, default: bool) -> bool {
    if cand == default { base } else { cand }
}

/// Extract config with every optional bit at its "off" default. Used as the
/// merge baseline: fields equal to `DEFAULT_CONFIG` count as unset during a
/// config merge.
const DEFAULT_CONFIG: ExtractConfig = ExtractConfig {
    force: false,
    paths: PathLayout {
        archive_path: PathBuf::new(),
        directory: PathBuf::new(),
        work_dir: PathBuf::new(),
    },
    decompression: super::compression::CompressionFormat::None,
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
        clean_target: false,
        link_source: None,
        no_reflink: false,
        hard_link_grouping: HardLinkGrouping::Global,
        recreate_none_file_entries: true,
    },
    attributes: ExtractAttributeOptions {
        restore_owner: false,
        no_overwrite_dir: false,
        force_overwrite_dir: false,
        apply_atime: false,
        apply_mtime: false,
        no_xattrs: true,
        no_acls: true,
        no_selinux: true,
        no_same_permissions: true,
    },
    scan: ScanOptions {
        force_scan: false,
        rehash: true,
        clear_archive_meta: false,
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
    owner_policy: OwnerGroupSource::None,
    owner_group: OwnerGroupOptions {
        target: MapResolutionTarget::NameId,
        validate_maps: false,
        same_owner: false,
        apply_owner: false,
        apply_group: false,
    },
    mode_policy: ModeSource::None,
    strip_components: 0,
    transform_policy: TransformSource::None,
    filter: FilterOptions {
        exclude_patterns: Vec::new(),
        include_patterns: Vec::new(),
        exclude_from: Vec::new(),
        include_from: Vec::new(),
        anchored: false,
        ignore_case: false,
        eager_filter: false,
    },
};
