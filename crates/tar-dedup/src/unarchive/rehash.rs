//! Rehash: recompute content hashes to detect corruption (stub).

use crate::config::Config;
use crate::db::Database;
use crate::error::Result;
use crate::shutdown::Shutdown;

pub fn run(_config: &Config, _db: &Database, _shutdown: &Shutdown) -> Result<()> {
    tracing::info!("unarchive rehash: no-op stub");
    Ok(())
}
