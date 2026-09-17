use crate::cli::ArchiveInputArgs;
use crate::cmd::{not_implemented, resolve_catalog_source};
use crate::error::Result;

/// List file-tree members (rows from the `files` table).
pub fn run(args: &ArchiveInputArgs) -> Result<()> {
    let _source = resolve_catalog_source(args)?;
    Err(not_implemented("list"))
}