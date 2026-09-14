//! Shared helpers used by both archive and unarchive pipelines.

pub mod cleanup;
pub mod files;
pub mod filter;
pub mod perms;
pub mod start;
pub mod transform;
pub mod xattr;

use crate::error::Result;

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

/// Number of ErrorRecordDrafts at a time in ram before attempting to auto flush;
pub const DEFAULT_AUTO_FLUSH_LIMIT: u64 = 10_000;

/// Perform the batched loop with a step id. Arguments work as follows:
/// [`new_id`]: Function must return the lower bound for ids. Typically 0, since we start id at 1
/// [`get_entries`]: Function that gets the next batch starting with last_id, u64 is for batch_size
/// [`get_id`]: Function gets the id from an entry. This function MUST return an id.
/// [`process_entries`]: Once the entries are ready, hand control to "loop body" function
/// [`batch_size`]: Determines the max size of batches from the get_entries function.
pub fn batched_stepped_loop<ID, ENTRY>(
    init_id: fn() -> ID,
    get_entries: fn(&ID, u64) -> Result<Vec<ENTRY>>,
    get_id: fn(&ENTRY) -> ID,
    process_entries: fn(Vec<ENTRY>) -> Result<()>,
    batch_size: u64)
    -> Result<()> {

    let mut last_id: ID = init_id();
    loop {
        let entries = get_entries(&last_id, batch_size)?;
        if entries.is_empty() { break }
        let vec_last = entries
            .last()
            .expect("PRECONDITION FAILED: At least one element expect.");
        last_id = get_id(vec_last);

        process_entries(entries)?;
    }
    Ok(())
}

pub fn batched_loop<ENTRY>(
    get_entries: fn(u64) -> Result<Vec<ENTRY>>,
    batch_size: u64,
    process_entries: fn(Vec<ENTRY>) -> Result<()>)
    -> Result<()> {

    loop {
        let entries = get_entries(batch_size)?;
        if entries.is_empty() { break }
        process_entries(entries)?;
    }
    Ok(())
}