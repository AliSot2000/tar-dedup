use crate::cli::ResetArgs;
use crate::cmd::{not_implemented, resolve_reset_db};
use crate::error::Result;

/// Update stored archive/extract parameters and reset pipeline state.
pub fn run(args: &ResetArgs) -> Result<()> {
    let _db = resolve_reset_db(args)?;
    Err(not_implemented("reset"))
}