use crate::config::Config;
use crate::db::Database;
use crate::error::Result;

pub fn run(_config: &Config, _db: &Database) -> Result<()> {
    Err(crate::error::Error::Other(anyhow::anyhow!(
        "extract pipeline not implemented yet"
    )))
}
