use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

use crate::common::files::{PreYield, warn_if_times_changed};
use crate::config::Config;
use crate::db::Database;
use crate::db::flags::FileFlag;
use crate::db::types::{FileId, StrippedRecord};
use crate::error::{Error, Result};
use crate::progress::io_buffer;
use crate::shutdown::Shutdown;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::ThreadPoolBuilder;
use rayon::prelude::*;
use sha1::{Digest, Sha1};

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    let page_size = config.page_size;
    debug_assert!(page_size > 0, "page_size == 0");

    let total_entries = db.count_entries()?;
    let hash_needed = db.count_all_hashable_files(
        config.eager_filter, !config.no_hardlink_detection
    )?;
    let pending= db.get_entries_to_hash(
        config.eager_filter, !config.no_hardlink_detection
    )?;
    let already_hashed = hash_needed.saturating_sub(pending.len() as u64);
    tracing::info!(
        total_entries,
        unshed_files = pending.len(),
        already_hashed,
        jobs = config.jobs,
        page_size,
        "hash pass"
    );

    if pending.is_empty() {
        return Ok(());
    }

    let pool = ThreadPoolBuilder::new()
        .num_threads(config.jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    let shutdown = shutdown.clone();
    let results = Mutex::new(
        Vec::<std::result::Result<(FileId, [u8; 20], u64), IdError>>::new());

    let bar = ProgressBar::new(hash_needed);
    bar.set_position(already_hashed);
    bar.set_style(
        ProgressStyle::with_template("{spinner} hash [{bar:40.cyan/blue}] {pos}/{len}")
            .unwrap()
            .progress_chars("=>-"),
    );
    bar.enable_steady_tick(std::time::Duration::from_millis(100));

    // `PreYield` stats each file when `par_bridge` pulls it for a worker — just
    // before that file is hashed, not in a bulk pass at the start of the stage.
    let checked = PreYield::new(pending.iter(), |record: &&StrippedRecord| {
        warn_if_times_changed(&record.abs_path, record.mtime, record.atime, record.ctime);
    });
    let parallel = pool.install(|| {
        checked.par_bridge().try_for_each(|record| {
            shutdown.check_between_files()?;
            let res = match hash_file(
                &record.abs_path, page_size, &shutdown){
                Ok((digest, zero_blocks)) => Ok((record.id, digest, zero_blocks)),
                Err(e) => Err(IdError{err: e, id: record.id}),
            };
            results.lock().expect("hash results lock").push(res);
            bar.inc(1);
            Ok(())
        })
    });

    // TODO flow system later on.
    let _future_vec = Vec::<std::result::Result<(FileId, [u8; 20], u64), IdError>>::new();
    let hashed = std::mem::replace(
        &mut *results.lock().expect("hash results lock"),
        _future_vec);

    for res in &hashed {
        match res {
            Ok((id, digest, zero_blocks)) => {
                db.update_file_inspection_per_id(*id, *digest, *zero_blocks, !config.no_hardlink_detection)?;
            }
            Err(e) => {
                let ra = db.set_flag(e.id, FileFlag::ErrorWhileHash, true)?;
                assert_eq!(ra, 1, "Rows affected must be 1. Got {ra}. \
                0 - row vanished, >1 id constraint violated.");
                // Todo capture error
                // Todo logging / error
            }
        }
    }

    let force = shutdown.is_force();

    // TODO dummy update of the remining entries.
    match parallel {
        Ok(()) => {
            bar.finish_with_message(format!("hashing complete ({hash_needed}/{hash_needed})"));
            tracing::info!(count = hashed.len(), "hashing complete");
            Ok(())
        }
        Err(Error::Interrupted) if force => {
            bar.abandon();
            tracing::warn!("hashing force-aborted; in-flight progress discarded");
            Err(Error::Interrupted)
        }
        Err(Error::Interrupted) => {
            bar.abandon();
            tracing::warn!(saved = hashed.len(), "hashing stopped; completed files saved");
            Err(Error::Interrupted)
        }
        Err(e) => Err(e),
    }
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
        shutdown.check_in_flight()?;
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
