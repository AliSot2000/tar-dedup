//! Apply permissions: mode, owner, xattrs, POSIX ACLs, SELinux (stub).

use crate::config::Config;
use crate::db::Database;
use crate::error::Result;
use crate::shutdown::Shutdown;

pub fn run(_config: &Config, _db: &Database, _shutdown: &Shutdown) -> Result<()> {
    tracing::info!("unarchive permissions: no-op stub");
    Ok(())
}
