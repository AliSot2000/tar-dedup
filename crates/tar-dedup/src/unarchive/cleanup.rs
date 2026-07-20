//! Cleanup: remove extract cache and temporary ingest files.

use std::fs;

use crate::config::Config;
use crate::error::{Error, Result};

pub fn run(config: &Config) -> Result<()> {
    let cache = config.extract_cache_dir();
    if cache.is_dir() {
        fs::remove_dir_all(&cache).map_err(|e| Error::io(&cache, e))?;
    }
    let tmp = config.work_dir.join(".snapshot-ingest.tmp");
    if tmp.is_file() {
        let _ = fs::remove_file(&tmp);
    }
    Ok(())
}

pub fn reset_extract_work(config: &Config) -> Result<()> {
    run(config)?;
    let db_path = config.db_path();
    if db_path.is_file() {
        fs::remove_file(&db_path).map_err(|e| Error::io(&db_path, e))?;
    }
    Ok(())
}
