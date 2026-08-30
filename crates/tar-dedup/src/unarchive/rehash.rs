//! Rehash: verify extract-cache payloads against catalog SHA-1 digests.

use std::fmt::format;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

use crate::config::ExtractConfig;
use crate::db::Database;
use crate::db::flags::FileFlag;
use crate::db::types::{FileId, FilePhase, StrippedRecord};
use crate::error::{Error, Result};
use crate::progress::io_buffer;
use crate::shutdown::Shutdown;
use indicatif::{ProgressBar, ProgressStyle};
use rayon::ThreadPoolBuilder;
use rayon::prelude::*;
use sha1::{Digest, Sha1};
use crate::unarchive::rehash::RehashOutcome::Error;

#[derive(Debug, Clone)]
enum RehashOutcome {
    /// Digest matches catalog `sha1` (or no payload to verify — duplicate row).
    Match(FileId),
    /// Digest differs from catalog `sha1`.
    Mismatch(FileId),
    /// IO / missing cache / missing expected digest.
    Error(FileId),
}

pub fn run(config: &ExtractConfig, db: &Database, shutdown: &Shutdown) -> Result<()> {
    // TODO promote all files that aren't elected to rehash
    let pending: Vec<StrippedRecord> = db.files_in_phase(FilePhase::Unarchived)?; // TODO that's wrong
    let total = pending.len() as u64;
    let already_hashed= 0;

    let do_skip = if config.scan.rehash { "" } else { "skip " };
    tracing::info!(
        files = pending.len(),
        jobs = config.process.jobs,
        "{do_skip}rehash pass"
    );

    if !config.scan.rehash {
        let n = db.skip_rehash()?;
        tracing::info!(promoted = n, "rehash skipped; unarchived → rehashed");
        return Ok(());
    }

    if pending.is_empty() {
        return Ok(());
    }

    let pool = ThreadPoolBuilder::new()
        .num_threads(config.process.jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    let stage_dir = config.paths.stage_dir();
    let shutdown = shutdown.clone();
    let results = Mutex::new(Vec::<RehashOutcome>::new());

    let bar = ProgressBar::new(total);
    bar.set_position(already_hashed); // TODO we need proper information
    bar.set_style(
        ProgressStyle::with_template("{spinner} rehash [{bar:40.cyan/blue}] {pos}/{len}")
            .unwrap()
            .progress_chars("=>-"),
    );
    bar.enable_steady_tick(std::time::Duration::from_millis(100));

    let parallel = pool.install(|| {
        pending.par_iter().try_for_each(|record| -> Result<()> {
            shutdown.check_between_files()?;

            let outcome = rehash_one(&stage_dir, record, &shutdown);
            results
                .lock()
                .expect("rehash results lock")
                .push(outcome);
            bar.inc(1);
            Ok(())
        })
    });

    let outcomes = results.lock().expect("rehash results lock").clone();
    let counts = stat_and_apply_outcomes(db, &outcomes)?;

    let force = shutdown.is_force();
    match parallel {
        Ok(()) => {
            bar.finish_with_message(format!("rehash complete ({total}/{total})"));
            let (matches, mismatches, errors) = counts;
            tracing::info!(matches, mismatches, errors, "rehash complete");
            if mismatches > 0 {
                if config.force {
                    tracing::warn!(mismatches, "rehash digest mismatch(es) recorded");
                } else {
                    return Err(Error::Config(format!(
                        "Corruption detected: {mismatches} files with mismatching hash. \
                        Ignore this error with --force")))
                }
            }
            if errors > 0 {
                tracing::warn!(errors, "rehash error(s) recorded");
            }
            Ok(())
        }
        Err(Error::Interrupted) if force => {
            bar.abandon();
            tracing::warn!("rehash force-aborted; in-flight progress discarded");
            Err(Error::Interrupted)
        }
        Err(Error::Interrupted) => {
            bar.abandon();
            tracing::warn!(
                saved = outcomes.len(),
                "rehash stopped; completed files saved"
            );
            Err(Error::Interrupted)
        }
        Err(e) => Err(e),
    }
}

/// Compute and compare the hash of a single canonical record
fn rehash_one(stage_dir: &Path, record: &StrippedRecord, shutdown: &Shutdown) -> RehashOutcome {
    let id = record.id;

    // Duplicates share the canonical cache payload; nothing to hash for this row.
    let Some(member) = record.tar_member_name() else {
        return RehashOutcome::Match(id);
    };

    let path = stage_dir.join(&member);
    let digest = match hash_file(&path, shutdown) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                file_id = id.0,
                path = %path.display(),
                error = %e,
                "rehash failed"
            );
            return RehashOutcome::Error(id);
        }
    };

    let Some(expected) = record.sha1 else {
        tracing::warn!(file_id = id.0, "rehash: catalog row has no sha1");
        return RehashOutcome::Error(id);
    };

    if digest == expected {
        RehashOutcome::Match(id)
    } else {
        RehashOutcome::Mismatch(id)
    }
}

/// Update the database from a single outcome
fn stat_and_apply_outcomes(db: &Database, outcomes: &[RehashOutcome]) -> Result<(u64, u64, u64)> {
    let mut matches = 0u64;
    let mut mismatches = 0u64;
    let mut errors = 0u64;
    for outcome in outcomes {
        match outcome {
            RehashOutcome::Match(id) => {
                db.mark_file_phase(*id, FilePhase::Rehashed)?;
                matches += 1;
            }
            RehashOutcome::Mismatch(id) => {
                db.set_flag(*id, FileFlag::RehashMismatch, true)?;
                db.mark_file_phase(*id, FilePhase::Rehashed)?;
                mismatches += 1;
            }
            RehashOutcome::Error(id) => {
                db.set_flag(*id, FileFlag::ErrorWhileRehashing, true)?;
                db.mark_file_phase(*id, FilePhase::Rehashed)?;
                errors += 1;
            }
        }
    }
    Ok((matches, mismatches, errors))
}

/// SHA-1 only (no sparse / hole accounting).
fn hash_file(path: &Path, shutdown: &Shutdown) -> Result<[u8; 20]> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
    let mut hasher = Sha1::new();
    let mut read_buf = io_buffer();

    loop {
        shutdown.check_in_flight()?;
        let n = file.read(&mut read_buf).map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&read_buf[..n]);
    }

    Ok(hasher.finalize().into())
}
