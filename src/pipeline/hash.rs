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

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    let pending = db.files_in_phase(crate::db::types::FilePhase::Inventoried)?;
    tracing::info!(files = pending.len(), jobs = config.jobs, "hash pass");

    if pending.is_empty() {
        return Ok(());
    }

    let pool = ThreadPoolBuilder::new()
        .num_threads(config.jobs)
        .build()
        .map_err(|e| Error::Other(anyhow::anyhow!("thread pool: {e}")))?;

    let input_dir = config.input_dir.clone();
    let shutdown = shutdown.clone();
    let results = Mutex::new(Vec::<(FileId, [u8; 20])>::new());

    let bar = ProgressBar::new(pending.len() as u64);
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
            let digest = hash_file(&path, &shutdown)?;
            results
                .lock()
                .expect("hash results lock")
                .push((record.id, digest));
            bar.inc(1);
            Ok(())
        })
    });

    bar.abandon();

    let hashed = results.lock().expect("hash results lock").clone();
    for (id, digest) in &hashed {
        db.set_sha1(*id, *digest)?;
    }

    let force = shutdown.is_force();

    match parallel {
        Ok(()) => {
            tracing::info!(count = hashed.len(), "hash complete");
            Ok(())
        }
        Err(Error::Interrupted) if force => {
            tracing::warn!("hash force-aborted; in-flight progress discarded");
            Err(Error::Interrupted)
        }
        Err(Error::Interrupted) => {
            tracing::warn!(saved = hashed.len(), "hash stopped; completed files saved");
            Err(Error::Interrupted)
        }
        Err(e) => Err(e),
    }
}

fn hash_file(path: &Path, shutdown: &Shutdown) -> Result<[u8; 20]> {
    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;

    let mut hasher = Sha1::new();
    let mut buf = io_buffer();
    loop {
        shutdown.check_in_flight()?;
        let n = file.read(&mut buf).map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}
