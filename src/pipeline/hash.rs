use std::fs::File;
use std::io::Read;
use std::path::Path;

use sha1::{Digest, Sha1};

use crate::config::Config;
use crate::db::types::FilePhase;
use crate::db::Database;
use crate::error::Result;
use crate::progress::{io_buffer, ByteProgress, CountProgress};

pub fn run(config: &Config, db: &Database) -> Result<()> {
    let pending = db.files_in_phase(FilePhase::Inventoried)?;
    tracing::info!(files = pending.len(), "hash pass");
    let progress = CountProgress::new("hash");

    for record in pending {
        let path = config.input_dir.join(&record.rel_path);
        let digest = hash_file(&path)?;
        db.set_sha1(record.id, digest)?;
        progress.inc(1);
    }

    progress.finish("hash complete");
    Ok(())
}

fn hash_file(path: &Path) -> Result<[u8; 20]> {
    let mut file = File::open(path).map_err(|e| crate::error::Error::io(path, e))?;
    let total = file
        .metadata()
        .map_err(|e| crate::error::Error::io(path, e))?
        .len();

    let show_file_progress = total >= 512 * 1024 * 1024;
    let mut progress = show_file_progress.then(|| {
        ByteProgress::new(&format!("hash {}", path.display()), Some(total))
    });

    let mut hasher = Sha1::new();
    let mut buf = io_buffer();
    let mut consumed = 0u64;
    loop {
        let n = file.read(&mut buf).map_err(|e| crate::error::Error::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        consumed += n as u64;
        if let Some(bar) = progress.as_mut() {
            bar.on_bytes(consumed);
        }
    }
    if let Some(bar) = progress.as_ref() {
        bar.finish();
    }
    Ok(hasher.finalize().into())
}
