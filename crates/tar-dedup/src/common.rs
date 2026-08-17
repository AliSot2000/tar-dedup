//! Shared helpers used by both archive and unarchive pipelines.

pub mod cleanup;
pub mod files;
pub mod start;
pub mod xattr;

// Constants reused across the project that need to be coherent.

/// Name for the first database that is added to an archive to record what was considered initially.
pub const SNAPSHOT_INIT_TAR_NAME: &str = "manifest.sqlite";

/// Name for any subsequent database added to the archive which are used to store the progress of
/// appending files to the archive.
pub const SNAPSHOT_TAR_NAME: &str = "snapshot.sqlite";