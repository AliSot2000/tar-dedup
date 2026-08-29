use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct PathSource {
    pub original_path: PathBuf,
    pub absolute_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct PathLayout {
    pub archive_path: PathBuf,
    /// Path-resolution base; extract extraction root.
    pub directory: PathBuf,
    pub work_dir: PathBuf,
}

impl PathLayout {
    pub fn db_path(&self) -> PathBuf {
        self.work_dir.join("tar-dedup.sqlite")
    }

    pub fn temp_db(&self) -> PathBuf {
        self.work_dir.join("temp-tar-dedup.sqlite")
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
        &self.directory
    }
}
