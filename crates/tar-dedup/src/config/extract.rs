use std::path::{Path, PathBuf};

use crate::cli::{ConflictPolicy, ExtractArgs};
use crate::common::start::StartPolicy;
use crate::error::{Error, Result};

use super::compression::infer_compression_from_suffix;
use super::paths::PathLayout;
use super::process::{CleanupSettings, ProcessOptions};
use super::{
    default_extract_work_dir, resolve_cwd, resolve_path_to_abs_path, ExtractStageLocation,
};

#[derive(Debug, Clone)]
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
    pub hardlink_reestablish: bool,
    pub clean_target: bool, // TODO implies no_create_dir is false!
    pub link_source: Option<PathBuf>,  // TODO CLI + san!!!
    pub no_reflink: bool // TODO cli
}

#[derive(Debug, Clone)]
pub struct ExtractAttributeOptions {
    pub restore_owner: bool,
    pub no_overwrite_dir: bool,
    pub force_overwrite_dir: bool,
}

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub force_scan: bool,
    pub rehash: bool,
    pub clear_archive_meta: bool,
}

#[derive(Debug, Clone)]
pub struct ExtractConfig {
    pub force: bool, // Will force the system just try anyway even tho stuff failed. Might lead to inconsistent states and corrupted data.
    pub paths: PathLayout,
    pub decompression: super::compression::CompressionFormat,
    pub placement: PlacementOptions,
    pub attributes: ExtractAttributeOptions,
    pub scan: ScanOptions,
    pub process: ProcessOptions,
}

impl ExtractConfig {
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
                hardlink_reestablish: args.hardlink_reestablish,
                clean_target: true,
                link_source: None,
                no_reflink: false,
            },
            attributes: ExtractAttributeOptions {
                restore_owner: args.restore_owner,
                no_overwrite_dir: args.no_overwrite_dir,
                force_overwrite_dir: args.force_overwrite_dir,
            },
            scan: ScanOptions {
                force_scan: false,
                rehash: true,
                clear_archive_meta: false,
            },
            process: ProcessOptions {
                start_policy,
                jobs: 1,
                fail_fast: args.fail_fast,
                no_errors: false,
                cleanup: CleanupSettings::from_flags(args.keep_db, args.keep_stage),
                exit_after_stage: None,
            },
        })
    }

    /// Minimal config for `resume` when extract runtime state is present.
    pub fn for_resume(work_dir: PathBuf, jobs: usize) -> Self {
        Self {
            force: true, // TODO cli
            paths: PathLayout {
                archive_path: PathBuf::new(),
                directory: PathBuf::new(),
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
                hardlink_reestablish: true,
                clean_target: true,
                link_source: None,
                no_reflink: false,
            },
            attributes: ExtractAttributeOptions {
                restore_owner: false,
                no_overwrite_dir: false,
                force_overwrite_dir: false,
            },
            scan: ScanOptions {
                force_scan: false,
                rehash: true,
                clear_archive_meta: false,
            },
            process: ProcessOptions {
                start_policy: StartPolicy::Resume,
                jobs,
                fail_fast: false,
                no_errors: false,
                cleanup: CleanupSettings::from_flags(false, false),
                exit_after_stage: None,
            },
        }
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
                hardlink_reestablish: true,
                clean_target: false,
                link_source: None,
                no_reflink: false,
            },
            attributes: ExtractAttributeOptions {
                restore_owner: false,
                no_overwrite_dir: false,
                force_overwrite_dir: false,
            },
            scan: ScanOptions {
                force_scan: false,
                rehash: true,
                clear_archive_meta: false,
            },
            process: ProcessOptions {
                start_policy: StartPolicy::Create,
                jobs,
                fail_fast: false,
                no_errors: false,
                cleanup: CleanupSettings::from_flags(false, false),
                exit_after_stage: None,
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
                hardlink_reestablish: true,
                clean_target: true, // TODO cli
                link_source: None,
                no_reflink: false,
            },
            attributes: ExtractAttributeOptions {
                restore_owner: false,
                no_overwrite_dir: false,
                force_overwrite_dir: false,
            },
            scan: ScanOptions {
                force_scan: false,
                rehash: true,
                clear_archive_meta: false,
            },
            process: ProcessOptions {
                start_policy: StartPolicy::Create,
                jobs: 1,
                fail_fast: false,
                no_errors: false,
                cleanup: CleanupSettings::from_flags(false, false),
                exit_after_stage: None,
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
