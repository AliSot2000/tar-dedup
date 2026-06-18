use crate::config::Config;
use crate::db::types::FileId;
use crate::db::Database;
use crate::error::Result;
use crate::shutdown::Shutdown;

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    for group in db.duplicate_groups()? {
        shutdown.check_between_files()?;

        let canonical = db
            .canonical_for(group.sha1, group.size)?
            .expect("group member exists");

        let canonical_path = resolve_path(config, db, canonical)?;
        for member in group.members {
            shutdown.check_between_files()?;

            if member == canonical {
                db.mark_self_canonical(member)?;
                continue;
            }

            let member_path = resolve_path(config, db, member)?;
            if files_equal(&canonical_path, &member_path, shutdown)? {
                db.set_canonical(member, canonical)?;
            } else {
                db.mark_self_canonical(member)?;
            }
        }
    }

    for record in db.files_in_phase(crate::db::types::FilePhase::Hashed)? {
        shutdown.check_between_files()?;
        db.mark_self_canonical(record.id)?;
    }

    Ok(())
}

fn resolve_path(config: &Config, db: &Database, file_id: FileId) -> Result<std::path::PathBuf> {
    let record = db
        .get_file(file_id)?
        .ok_or_else(|| crate::error::Error::Config(format!("missing file id {}", file_id.0)))?;
    Ok(config.input_dir.join(record.rel_path))
}

fn files_equal(
    a: &std::path::Path,
    b: &std::path::Path,
    shutdown: &Shutdown,
) -> Result<bool> {
    use std::fs::File;
    use std::io::Read;

    let mut fa = File::open(a).map_err(|e| crate::error::Error::io(a, e))?;
    let mut fb = File::open(b).map_err(|e| crate::error::Error::io(b, e))?;
    if fa.metadata()?.len() != fb.metadata()?.len() {
        return Ok(false);
    }

    let mut buf_a = [0u8; 1024 * 1024];
    let mut buf_b = [0u8; 1024 * 1024];
    loop {
        shutdown.check_in_flight()?;
        let na = fa.read(&mut buf_a)?;
        let nb = fb.read(&mut buf_b)?;
        if na == 0 && nb == 0 {
            return Ok(true);
        }
        if na != nb || buf_a[..na] != buf_b[..nb] {
            return Ok(false);
        }
    }
}
