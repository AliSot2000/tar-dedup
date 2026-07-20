use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;
use sha1::{Digest, Sha1};

use crate::config::Config;
use crate::db::types::FileId;
use crate::db::Database;
use crate::error::{Error, Result};
use crate::progress::io_buffer;
use crate::shutdown::Shutdown;

/// Block size used when counting all-zero stretches during the hash pass.
/// Independent of the I/O read buffer; CLI wiring comes later.
const ZERO_BLOCK_SIZE: usize = 4096;

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    let total = db.count_files()?;
    let pending = db.files_in_phase(crate::db::types::FilePhase::Inventoried)?;
    let already_hashed = total.saturating_sub(pending.len() as u64);
    tracing::info!(
        files = pending.len(),
        total,
        already_hashed,
        jobs = config.jobs,
        zero_block_size = ZERO_BLOCK_SIZE,
        "hash pass"
    );

    if pending.is_empty() {
        return Ok(());
    }

    let pool = ThreadPoolBuilder::new()
        .num_threads(config.jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    let input_dir = config.input_dir.clone();
    let shutdown = shutdown.clone();
    let results = Mutex::new(Vec::<(FileId, [u8; 20], u64)>::new());

    let bar = ProgressBar::new(total);
    bar.set_position(already_hashed);
    bar.set_style(
        ProgressStyle::with_template("{spinner} hash [{bar:40.cyan/blue}] {pos}/{len}")
            .unwrap()
            .progress_chars("=>-"),
    );
    bar.enable_steady_tick(std::time::Duration::from_millis(100));

    let parallel = pool.install(|| {
        pending.par_iter().try_for_each(|record| {
            shutdown.check_between_files()?;
            let path = input_dir.join(&record.rel_path);
            let (digest, zero_blocks) = hash_file(&path, &shutdown)?;
            results
                .lock()
                .expect("hash results lock")
                .push((record.id, digest, zero_blocks));
            bar.inc(1);
            Ok(())
        })
    });

    let hashed = results.lock().expect("hash results lock").clone();
    for (id, digest, zero_blocks) in &hashed {
        db.update_file_inspection(*id, *digest, *zero_blocks)?;
    }

    let force = shutdown.is_force();

    match parallel {
        Ok(()) => {
            bar.finish_with_message(format!("hash complete ({total}/{total})"));
            tracing::info!(count = hashed.len(), "hash complete");
            Ok(())
        }
        Err(Error::Interrupted) if force => {
            bar.abandon();
            tracing::warn!("hash force-aborted; in-flight progress discarded");
            Err(Error::Interrupted)
        }
        Err(Error::Interrupted) => {
            bar.abandon();
            tracing::warn!(saved = hashed.len(), "hash stopped; completed files saved");
            Err(Error::Interrupted)
        }
        Err(e) => Err(e),
    }
}

/// Single-pass SHA-1 and empty-block count.
///
/// Bytes are hashed as read. Separately, the stream is partitioned into fixed
/// [`ZERO_BLOCK_SIZE`] windows (independent of the I/O buffer). Only **full**
/// all-zero windows count; a short trailing window does not (same rule as
/// `sparse-cp::sparse_page_count`).
///
/// Zero checks slice `read_buf` in place. Across a read boundary we only keep
/// `carry_len` / `carry_zero` — never the leftover bytes themselves.
fn hash_file(path: &Path, shutdown: &Shutdown) -> Result<([u8; 20], u64)> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;

    let mut hasher = Sha1::new();
    let mut read_buf = io_buffer();
    let mut zero_blocks = 0u64;
    // Incomplete block spanning the previous read: length so far, and whether
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
            let need = ZERO_BLOCK_SIZE - carry_len;
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
        while i + ZERO_BLOCK_SIZE <= n {
            if is_all_zero(&read_buf[i..i + ZERO_BLOCK_SIZE]) {
                zero_blocks += 1;
            }
            i += ZERO_BLOCK_SIZE;
        }

        // Scan remaining block for zeros.
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
