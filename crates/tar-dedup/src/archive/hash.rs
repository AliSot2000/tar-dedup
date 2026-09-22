use crate::archive::ArchiveRTArgs;
use crate::common;
use crate::common::IO_BUF_SIZE;
use crate::common::files::warn_if_times_changed;
use crate::db::flags::FileFlag;
use crate::db::types::{FileId, StrippedRecord};
use crate::error::{Error, Result};
use crate::progress::BarKind;
use crate::shutdown::Shutdown;
use common::batched_loop;
use indicatif::ProgressBar;
use rayon::ThreadPoolBuilder;
use rayon::prelude::*;
use sha1::{Digest, Sha1};
use std::cell::RefCell;
use std::fs::File;
use std::io::Read;

// TODO via args
const BATCH_SIZE: u64 = 10_000;

pub fn run(rt: &ArchiveRTArgs) -> Result<()> {
    let db = rt.db;
    let shutdown = rt.shutdown;
    debug_assert!(rt.config.sparse.page_size > 0, "page_size == 0");

    let total_entries = db.count_entries()?;
    let hash_needed = db.count_all_hashable_files(
        rt.config.filter.eager_filter,
        !rt.config.indexing.no_hardlink_detection,
    )?;
    let pending = db.count_pending_hashable_files(
        rt.config.filter.eager_filter,
        !rt.config.indexing.no_hardlink_detection,
    )?;
    let already_hashed = hash_needed.saturating_sub(pending);
    tracing::info!(
        total_entries,
        unshed_files = pending,
        already_hashed,
        jobs = rt.config.process.effective_jobs(),
        page_size = rt.config.sparse.page_size,
        "hash pass"
    );

    rt.progress.set_phase_total(hash_needed);
    rt.progress.set_phase_position(already_hashed);
    let promoted_entries = db.promote_unhasheable_files(
        rt.config.filter.eager_filter,
        !rt.config.indexing.no_hardlink_detection)?;
    tracing::info!("Promoted {promoted_entries} entries which cannot be hashed.");
    rt.progress.inc_global(total_entries - hash_needed);

    if pending == 0 {
        return Ok(());
    }

    let mut recorder = crate::db::Recorder::new(db, !rt.config.process.no_errors);
    let shutdown = shutdown.clone();
    let results = Mutex::new(Vec::<std::result::Result<(FileId, [u8; 20], u64), IdError>>::new());
    let pool = ThreadPoolBuilder::new()
        .num_threads(rt.config.process.effective_jobs())
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    // Method to pull next batch of entries from the db
    let pull_next_entries = |batch_size| db.get_entries_to_hash(
        rt.config.filter.eager_filter, !rt.config.indexing.no_hardlink_detection, batch_size);

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
fn record_hash_error(recorder: &mut crate::db::Recorder, e: &IdError) {
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

/// Worker thread: pulls one file at a time, hashes it, forwards the outcome.
/// Owns its bar and read buffer; only touches the channels and `tracing`.
fn hash_worker(
    bar: ProgressBar,
    page_size: usize,
    shutdown: Shutdown,
    work: Receiver<StrippedRecord>,
    out: Sender<HashingOutcome>,
) {
    let mut buf = Vec::<u8>::new();
    loop {
        match work.recv() {
            Ok(row) => {
                if shutdown.check_between_files().is_err() {
                    break;
                }
                bar.reset();
                bar.set_length(row.size);
                bar.set_message(format!("Hashing {}", row.abs_path.display()));
                warn_if_times_changed(&row.abs_path, row.mtime, row.atime, row.ctime);
                let res = hash_one(&mut buf, &row.abs_path, page_size, &shutdown, Some(&bar))
                    .map(|(digest, zero_blocks)| (row.id, digest, zero_blocks))
                    .map_err(|err| IdError { err, id: row.id });
                out.send(res).expect("hash worker: result channel closed");
            }
            Err(_) => break
        }
    }
}

/// Single-pass SHA-1 and empty-page count.
///
/// `buf` is the worker's reusable read buffer (sized once here); `pb`
/// advances live per buffer so a huge file's bar stays responsive.
///
/// Bytes are hashed as read. Separately, the stream is partitioned into fixed
/// `page_size` windows (independent of the I/O buffer). Only **full**
/// all-zero windows count; a short trailing window does not (same rule as
/// `sparse-cp::sparse_page_count`).
///
/// Zero checks slice `buf` in place. Across a read boundary we only keep
/// `carry_len` / `carry_zero` — never the leftover bytes themselves.
fn hash_one(
    buf: &mut Vec<u8>,
    path: &Path,
    page_size: usize,
    shutdown: &Shutdown,
    pb: Option<&ProgressBar>,
) -> Result<([u8; 20], u64)> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
    if buf.len() < IO_BUF_SIZE {
        buf.resize_with(IO_BUF_SIZE, || 0u8);
    }

    let mut hasher = Sha1::new();
    let mut zero_blocks = 0u64;
    // Incomplete page spanning the previous read: length so far, and whether
    // those bytes were all zero. `carry_len > 0` is the "cut off by buffer" flag.
    let mut carry_len = 0usize;
    let mut carry_zero = true;

    loop {
        match shutdown.check_in_flight() {
            Ok(_) => (),
            Err(e) => {
                tracing::info!("hash worker got in-flight interrupt");
                return Err(e);
            }
        };
        let n = file.read(buf).map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);

        let mut i = 0usize;

        // Handle segmentation between read_bufs
        if carry_len > 0 {
            let need = page_size - carry_len;
            if n < need {
                carry_zero &= is_all_zero(&buf[..n]);
                carry_len += n;
                continue;
            }
            if carry_zero && is_all_zero(&buf[..need]) {
                zero_blocks += 1;
            }
            carry_len = 0;
            carry_zero = true;
            i = need;
        }

        // Scan contiguous buffer
        while i + page_size <= n {
            if is_all_zero(&buf[i..i + page_size]) {
                zero_blocks += 1;
            }
            i += page_size;
        }

        // Scan remaining page for zeros.
        let rem = n - i;
        if rem > 0 {
            carry_len = rem;
            carry_zero = is_all_zero(&buf[i..n]);
        }
        if let Some(pb) = pb {
            pb.inc(n as u64);
        }
    }

    Ok((hasher.finalize().into(), zero_blocks))
}

#[inline]
fn is_all_zero(chunk: &[u8]) -> bool {
    chunk.iter().all(|&b| b == 0)
}
