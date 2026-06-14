use std::fs::File;
use std::io::Read;
use std::path::PathBuf;

use sha1::{Digest, Sha1};

use crate::config::Config;
use crate::db::types::FilePhase;
use crate::db::Database;
use crate::error::Result;
use crate::progress::ByteProgress;

pub fn run(config: &Config, db: &Database) -> Result<()> {
    let pending = db.files_in_phase(FilePhase::Inventoried)?;
    tracing::info!(files = pending.len(), "hash pass");

    for record in pending {
        let path = config.input_dir.join(&record.rel_path);
        let digest = hash_file(&path)?;
        db.set_sha1(record.id, digest)?;
    }

    Ok(())
}

fn hash_file(path: &PathBuf) -> Result<[u8; 20]> {
    let mut file = File::open(path).map_err(|e| crate::error::Error::io(path, e))?;
    let total = file.metadata().map_err(|e| crate::error::Error::io(path, e))?.len();
    let mut progress = ByteProgress::new(&format!("hash {}", path.display()), Some(total));

    let mut hasher = Sha1::new();
    let mut buf = [0u8; 8 * 1024 * 1024];
    let mut consumed = 0u64;
    loop {
        let n = file.read(&mut buf).map_err(|e| crate::error::Error::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        consumed += n as u64;
        progress.on_bytes(consumed);
    }
    progress.finish();
    Ok(hasher.finalize().into())
}
