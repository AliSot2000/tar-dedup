use crate::config::Validate;
use crate::error::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
        }
    }
}

impl Validate for FilterOptions {
    // INFO: Currently a stub to prevent bugs that result from not calling a subsequent
    //   implementation of this trait for this Struct
    fn validate(&self) -> Result<()> {
        Ok(())
    }
}