use std::fs;
use std::path::PathBuf;
use regex::Regex;
use crate::db::Database;
use crate::error::Result;
use crate::config::Config;

/// Stub filter stage: advance hashed → filtered before dedup.
pub fn run(db: &Database) -> Result<()> {
    let promoted = db.promote_hashed_to_filtered()?;
    if promoted > 0 {
        tracing::info!(count = promoted, "promoted hashed → filtered");
    }
    Ok(())
}
