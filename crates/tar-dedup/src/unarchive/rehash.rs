//! Rehash: recompute content hashes to detect corruption (stub).

use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;
use sha1::{Digest, Sha1};

use crate::common::files::{warn_if_times_changed, PreYield};
use crate::config::Config;
use crate::db::types::{FileId, FilePhase, StrippedRecord};
use crate::db::Database;
use crate::error::{Error, Result};
use crate::progress::io_buffer;
use crate::shutdown::Shutdown;

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    // TODO promote all files that aren't elected to rehash
    let total = db.count_files()?; // TODO accurate number
    let pending: Vec<StrippedRecord> = db.files_in_phase(FilePhase::Unarchived)?; // TODO accurate number.
    let already_hashed = total.saturating_sub(pending.len() as u64);

    let do_skip = if config.rehash { "" } else { "skip " };

    tracing::info!(
        files = pending.len(),
        total,
        already_hashed,
        jobs = config.jobs,
        "{do_skip}rehash pass"
    );

    if !config.rehash {
        db.skip_rehash()?; // TODO implement
    }

    if pending.is_empty() {
        return Ok(());
    }

    let pool = ThreadPoolBuilder::new()
        .num_threads(config.jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    let source_dir= config.stage_dir().clone();
    let shutdown = shutdown.clone();
    let results = Mutex::new(Vec::<(FileId, [u8; 20], u64)>::new());

    let bar = ProgressBar::new(total);
    bar.set_position(already_hashed);
    bar.set_style(
        ProgressStyle::with_template("{spinner} rehash [{bar:40.cyan/blue}] {pos}/{len}")
            .unwrap()
            .progress_chars("=>-"),
    );
    bar.enable_steady_tick(std::time::Duration::from_millis(100));

    let parallel = pool.install(|| {
            shutdown.check_between_files()?;
            let path = source_dir.join(&record.rel_path);
            let digest = hash_file(&path, &shutdown)?;
            results
                .lock()
                .expect("hash results lock")
                .push((record.id, digest, zero_blocks));
            bar.inc(1);
            Ok(())

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

/// Single-pass SHA-1
///
/// Bytes are hashed as read.
fn hash_file(path: &Path, shutdown: &Shutdown) -> Result<[u8; 20]> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;

    let mut hasher = Sha1::new();
    let mut read_buf = io_buffer();

    loop {
        shutdown.check_in_flight()?;
        let n = file.read(&mut read_buf).map_err(|e| Error::io(path, e))?;
        if n == 0 { break; }
        hasher.update(&read_buf[..n]);
    }

    Ok(hasher.finalize().into())
}