use crate::archive::ArchiveRTArgs;
use crate::common::files::{PreYield, warn_if_times_changed};
use crate::common::io_buffer;
use crate::db::flags::FileFlag;
use crate::db::types::{FileId, StrippedRecord};
use crate::error::{Error, Result};
use crate::shutdown::Shutdown;
use rayon::ThreadPoolBuilder;
use rayon::prelude::*;
use sha1::{Digest, Sha1};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;
use common::batched_loop;
use crate::common;

// TODO via args
const BATCH_SIZE: u64 = 10_000;

pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    let config = rt.config;
    let db = rt.db;
    let shutdown = rt.shutdown;
    let progress = rt.progress;
    let page_size = config.sparse.page_size;
    debug_assert!(page_size > 0, "page_size == 0");

    let total_entries = db.count_entries()?;
    let hash_needed = db.count_all_hashable_files(
        config.filter.eager_filter,
        !config.indexing.no_hardlink_detection,
    )?;
    let pending = db.count_pending_hashable_files(
        config.filter.eager_filter,
        !config.indexing.no_hardlink_detection,
    )?;
    let already_hashed = hash_needed.saturating_sub(pending);
    tracing::info!(
        total_entries,
        unshed_files = pending,
        already_hashed,
        jobs = config.process.effective_jobs(),
        page_size,
        "hash pass"
    );

    progress.set_phase_total(hash_needed);
    progress.set_phase_position(already_hashed);
    let promoted_entries = db.promote_unhasheable_files(
        rt.config.filter.eager_filter,
        !config.indexing.no_hardlink_detection)?;
    tracing::info!("Promoted {promoted_entries} entries which cannot be hashed.");
    progress.inc_global(total_entries - hash_needed);

    if pending == 0 {
        return Ok(());
    }

    let mut recorder = crate::db::Recorder::new(db, !config.process.no_errors);
    let shutdown = shutdown.clone();
    let results = Mutex::new(Vec::<std::result::Result<(FileId, [u8; 20], u64), IdError>>::new());
    let pool = ThreadPoolBuilder::new()
        .num_threads(config.process.effective_jobs())
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    // Method to pull next batch of entries from the db
    let pull_next_entries = |batch_size| db.get_entries_to_hash(
        config.filter.eager_filter, !config.indexing.no_hardlink_detection, batch_size);

    // Method to process the next batch of entries from the db.
    let loop_iteration = |entries: Vec<StrippedRecord>| {
        // `PreYield` stats each file when `par_bridge` pulls it for a worker — just
        // before that file is hashed, not in a bulk pass at the start of the stage.
        let checked = PreYield::new(
            entries.iter(),
            |record: &&StrippedRecord| {
                warn_if_times_changed(&record.abs_path, record.mtime, record.atime, record.ctime);
            }
        );
        let parallel = pool.install(|| {
            checked.par_bridge().try_for_each(|record| {
                shutdown.check_between_files()?;
                let res = match hash_file(&record.abs_path, page_size, &shutdown) {
                    Ok((digest, zero_blocks)) => Ok((record.id, digest, zero_blocks)),
                    Err(e) => Err(IdError { err: e, id: record.id }),
                };
                results.lock().expect("hash results lock").push(res);
                progress.inc_both(1);
                Ok(())
            })
        });

        let _future_vec = Vec::<std::result::Result<(FileId, [u8; 20], u64), IdError>>::new();
        let hashed = std::mem::replace(
            &mut *results.lock().expect("hash results lock"),
            _future_vec);

        for res in &hashed {
            match res {
                Ok((id, digest, zero_blocks)) => {
                    db.update_file_inspection_per_id(
                        *id,
                        *digest,
                        *zero_blocks,
                        !config.indexing.no_hardlink_detection,
                    )?;
                }
                Err(e) => {
                    let ra = db.set_file_flag(e.id, FileFlag::ErrorWhileHash, true)?;
                    assert_eq!(ra, 1, "Rows affected must be 1. Got {ra}. \
                0 - row vanished, >1 id constraint violated.");
                    record_hash_error(&mut recorder, &e);
                }
            }
        }

        // Handle rayon pool result
        match parallel {
            Ok(()) => {
                tracing::info!(count = hashed.len(), "hashing complete");
                Ok(())
            }
            Err(Error::Interrupted) if shutdown.is_force() => {
                tracing::warn!("hashing force-aborted; in-flight progress discarded");
                Err(Error::Interrupted)
            }
            Err(Error::Interrupted) => {
                tracing::warn!(saved = hashed.len(), "hashing stopped; completed files saved");
                Err(Error::Interrupted)
            }
            Err(e) => Err(e),
        }
    };

    batched_loop(pull_next_entries, BATCH_SIZE, loop_iteration)?;

    recorder.flush()?;

    let double_canonical = db.count_double_canonical_dev_inode_group()?;
    if double_canonical > 0 {
        panic!("Encountered {double_canonical} Hard Link files. \
            which have two different hashes. Assuming files modified while hashing.");
    }
    Ok(())
}

/// Record a per-file hash failure in the persistent error log (best-effort).
/// `hash_file` failures are `FileStat` (per-file) errors; the carried
/// [`FileStatError`](crate::error::FileStatError) is recreated on the way in.
fn record_hash_error(recorder: &mut crate::db::Recorder, e: &&IdError) {
    recorder.record_file(
        e.id,
        crate::db::ErrorPhase::Pipeline(crate::config::PipelinePhase::Hash),
        e.err.to_file_stat(None), // TODO move this outside.
        crate::db::flags::ErrorFlags::default(),
    );
}

struct IdError {
    err: Error,
    id: FileId,
}

/// Single-pass SHA-1 and empty-page count.
///
/// Bytes are hashed as read. Separately, the stream is partitioned into fixed
/// `page_size` windows (independent of the I/O buffer). Only **full**
/// all-zero windows count; a short trailing window does not (same rule as
/// `sparse-cp::sparse_page_count`).
///
/// Zero checks slice `read_buf` in place. Across a read boundary we only keep
/// `carry_len` / `carry_zero` — never the leftover bytes themselves.
fn hash_file(path: &Path, page_size: usize, shutdown: &Shutdown) -> Result<([u8; 20], u64)> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;

    let mut hasher = Sha1::new();
    let mut read_buf = io_buffer();
    let mut zero_blocks = 0u64;
    // Incomplete page spanning the previous read: length so far, and whether
    // those bytes were all zero. `carry_len > 0` is the "cut off by buffer" flag.
    let mut carry_len = 0usize;
    let mut carry_zero = true;

    loop {
        match shutdown.check_in_flight() {
            Ok(_) => (),
            Err(e) => {
                tracing::info!("Rayon Thread got In Flight Interrupt");
                return Err(e);
            }
        };
        let n = file.read(&mut read_buf).map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&read_buf[..n]);

        let mut i = 0usize;

        // Handle segmentation between read_bufs
        if carry_len > 0 {
            let need = page_size - carry_len;
            if n < need {
                carry_zero &= is_all_zero(&read_buf[..n]);
                carry_len += n;
                continue;
            }
            if carry_zero && is_all_zero(&read_buf[..need]) {
                zero_blocks += 1;
            }
            carry_len = 0;
            carry_zero = true;
            i = need;
        }

        // Scan contiguous buffer
        while i + page_size <= n {
            if is_all_zero(&read_buf[i..i + page_size]) {
                zero_blocks += 1;
            }
            i += page_size;
        }

        // Scan remaining page for zeros.
        let rem = n - i;
        if rem > 0 {
            carry_len = rem;
            carry_zero = is_all_zero(&read_buf[i..n]);
        }
    }

    Ok((hasher.finalize().into(), zero_blocks))
}

#[inline]
fn is_all_zero(chunk: &[u8]) -> bool {
    chunk.iter().all(|&b| b == 0)
}
