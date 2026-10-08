//! Shared defaults for the whole project.
//!
//! Central home for the constants we tune (channel bounds, batch sizes, buffer
//! sizes, tar member names). Prefer referencing these instead of scattering
//! magic numbers across phases; the config layer overlays CLI overrides on top
//! (see `config::process::ProcessOptions`).

/// Name for the first database that is added to an archive to record what was
/// considered initially.
pub const SNAPSHOT_INIT_TAR_NAME: &str = "manifest.sqlite";

/// Name for any subsequent database added to the archive which are used to store
/// the progress of appending files to the archive.
pub const SNAPSHOT_TAR_NAME: &str = "snapshot.sqlite";

/// To ensure the program is responsive, we need to periodically check, if the
/// user interrupted us. This is the stepsize during read / write operations
/// between successive checks of the program status.
pub const IO_BUF_SIZE: usize = 1024 * 1024 * 4;

/// Tar read chunk size during archive (keep xz fed without huge resident buffers).
pub const ARCHIVE_IO_BUF_SIZE: usize = 4 * 1024 * 1024;

/// Number of ErrorRecordDrafts at a time in ram before attempting to auto flush.
pub const DEFAULT_AUTO_FLUSH_LIMIT: u64 = 1_024 * 16;

/// Default batch size for per-phase SQL pulls (rows fetched per statement).
/// Kept modest: a worst-case [`crate::db::types::FileRecord`] (4 KiB xattr +
/// acl + selinux blobs, ~1 KiB paths) is ~21 KiB, so 10_000 rows ≈ 210 MiB —
/// safe even on a 4 GiB machine, where 100_000 rows would approach 2 GiB.
pub const BATCH_SIZE: u64 = 1_024 * 16;

// Producer/consumer pipeline bounds shared by every `send_receive_loop` phase
// (hash, dedup, sparsify, rehash, place). Bounds are generous so big in-flight
// files do not stall the feed; each phase still commits outcomes in
// `DRAIN_CHUNK`-sized transactions.

/// Items in the input (work) channel.
pub const WORK_CAPACITY: usize = 1_024 * 16;
/// Outcomes in the result channel.
pub const OUT_CAPACITY: usize = 1_024 * 32;
/// Rows pulled from the DB per feed round.
pub const FEED_CHUNK: usize = 1_024;
/// Outcomes committed per transaction.
pub const DRAIN_CHUNK: usize = 1_024 * 4;