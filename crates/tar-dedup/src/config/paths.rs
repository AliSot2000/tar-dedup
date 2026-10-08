use crate::config::Validate;
use path_clean::PathClean;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathSource {
    pub original_path: PathBuf,
    pub absolute_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathLayout {
    pub archive_path: PathBuf,
    /// Path-resolution base for relative CLI paths (`-C`).
    pub directory: PathBuf,
    /// Extraction root (restore target on extract; unused on archive).
    pub output_dir: PathBuf,
    pub work_dir: PathBuf,
}

impl Default for PathLayout {
    fn default() -> Self {
        Self {
            archive_path: PathBuf::new(),
            directory: PathBuf::new(),
            output_dir: PathBuf::new(),
            work_dir: PathBuf::new(),
        }
    }
}

impl Validate for PathLayout {
    fn validate(&self) -> crate::error::Result<()> {
        debug_assert_eq!(self.archive_path, self.archive_path.clean(),
                         "PRECONDITION ERROR: All paths are expected to be clean: archive_path");
        debug_assert_eq!(self.directory, self.directory.clean(),
                         "PRECONDITION ERROR: All paths are expected to be clean: directory");
        debug_assert_eq!(self.output_dir, self.output_dir.clean(),
                         "PRECONDITION ERROR: All paths are expected to be clean: output_dir");
        debug_assert_eq!(self.work_dir, self.work_dir.clean(),
                         "PRECONDITION ERROR: All paths are expected to be clean: work_dir");
        Ok(())
    }
}

impl PathLayout {
    pub fn db_path(&self) -> PathBuf {
        self.work_dir.join("tar-dedup.sqlite")
    }

    pub fn temp_db(&self) -> PathBuf {
        self.work_dir.join("temp-tar-dedup.sqlite")
    }

    pub fn temp_snapshot(&self) -> PathBuf {
        self.work_dir.join(".snapshot-ingest.tmp")
    }

    pub fn stage_archive_snapshot(&self) -> PathBuf {
        self.work_dir.join(".snapshot-for-tar.sqlite")
    }

    /// Archive payload directory — same as `work_dir` (flat `.astage`).
    pub fn stage_dir(&self) -> PathBuf {
        self.work_dir.clone()
    }

    /// Extract payload directory — same as `work_dir` (flat `.estage`).
    pub fn extract_cache_dir(&self) -> PathBuf {
        self.work_dir.clone()
    }

    pub fn extraction_root(&self) -> &Path {
        &self.output_dir
    }
}
