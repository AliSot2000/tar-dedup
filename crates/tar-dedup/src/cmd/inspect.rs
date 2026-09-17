use crate::cli::ArchiveInputArgs;
use crate::cmd::{not_implemented, resolve_catalog_source};
use crate::error::Result;

/// Show archive metadata needed to plan an extraction (catalog, policies).
pub fn run(args: &ArchiveInputArgs) -> Result<()> {
    let _source = resolve_catalog_source(args)?;
    Err(not_implemented("inspect"))
}