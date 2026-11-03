use crate::cli::FilterFlags;
use crate::config::{Validate, resolve_path_to_abs_path, validate_file};
use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilterOptions {
    pub exclude_patterns: Vec<String>,
    pub include_patterns: Vec<String>,
    pub exclude_from: Vec<PathBuf>,
    pub include_from: Vec<PathBuf>,
    pub anchored: bool,
    pub ignore_case: bool,
    pub eager_filter: bool,
    /// Compile the filter regexes in bytes mode (`\w`/`.` ASCII-only). Default:
    /// Unicode-aware matching.
    pub no_unicode: bool,
}

impl FilterOptions {
    pub fn build(filter_args: &FilterFlags, cwd: &Path, lazy_filter: bool) -> Self {
        let c_include_from: Vec<PathBuf> = filter_args
            .include_from
            .iter()
            .map(|p| resolve_path_to_abs_path(p, &cwd))
            .collect();
        let c_exclude_from: Vec<PathBuf> = filter_args
            .exclude_from
            .iter()
            .map(|p| resolve_path_to_abs_path(p, &cwd))
            .collect();

        Self {
            exclude_patterns: filter_args.exclude.clone(),
            include_patterns: filter_args.include.clone(),
            exclude_from: c_exclude_from,
            include_from: c_include_from,
            anchored: filter_args.anchored,
            ignore_case: filter_args.ignore_case,
            no_unicode: filter_args.no_unicode,
            eager_filter: lazy_filter,
        }
    }
}

impl Default for FilterOptions {
    fn default() -> Self {
        Self {
            exclude_patterns: Vec::new(),
            include_patterns: Vec::new(),
            exclude_from: Vec::new(),
            include_from: Vec::new(),
            anchored: false,
            ignore_case: false,
            eager_filter: true,
            no_unicode: false,
        }
    }
}

impl Validate for FilterOptions {
    fn validate(&self) -> Result<()> {
        self.include_from
            .iter()
            .map(|p| validate_file(p, "--include-from"))
            .collect::<Result<()>>()?;
        self.exclude_from
            .iter()
            .map(|p| validate_file(p, "--exclude-from"))
            .collect::<Result<()>>()?;
        Ok(())
    }
}