use crate::config::Config;
use crate::db::Database;
use crate::error::{Error, Result};
use crate::shutdown::Shutdown;

/// Stub sparsify stage: advance `deduped` → `sparsified`.
pub fn run(config: &Config, db: &Database, _shutdown: &Shutdown) -> Result<()> {
    assert!(config.page_size > 0, "Page Size cannot be zero");

    tracing::info!(
        page_size = config.page_size,
        min_pages = ?config.min_pages,
        "sparsify pass"
    );

    let n = db.promote_deduped_to_sparsified()?;
    tracing::info!(count = n, "promoted deduped → sparsified");
    Ok(())
}
