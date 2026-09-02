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

/// To ensure the program is responsive, we need to periodically check, if the user interrupted us.
/// This is the stepsize during read / write operations between successive checks of the program
/// status.
pub const COPY_STEP_SIZE: u64 = 1024 * 1024 * 4;

/// When processing files, file system entries, ... we take the precaution not to load too much
/// into ram. Worst case Estimate is 16kiB / Entry, so we try to be conservative with 100'000 as
/// a batch size
pub const DEFAULT_BATCH_SIZE: u64 = 100_000;