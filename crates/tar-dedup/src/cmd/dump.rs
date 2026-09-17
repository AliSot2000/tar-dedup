use crate::cli::ArchiveInputArgs;
use crate::cmd::{not_implemented, resolve_catalog_source};
use crate::error::Result;

/// Machine-formatted (csv/json) dump of the file-tree members.
pub fn run(args: &ArchiveInputArgs) -> Result<()> {
    let _source = resolve_catalog_source(args)?;
    Err(not_implemented("dump"))
}