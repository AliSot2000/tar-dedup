use crate::common::files::warn_if_times_changed;
use crate::config::Config;
use crate::db::types::FileId;
use crate::db::Database;
use crate::error::{Error, Result};
use crate::progress::{io_buffer, CountProgress};
use crate::shutdown::Shutdown;

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    let total = db.count_files()?;
    let pending_hashed = db.files_in_phase(crate::db::types::FilePhase::Hashed)?;
    let already = total.saturating_sub(pending_hashed.len() as u64);

    if total == already {
        return Ok(());
    }

    let bar = CountProgress::with_total("dedup", total);
    bar.set_position(already);

    let result = run_inner(config, db, shutdown, &bar);

    let force = shutdown.is_force();
    match result {
        Ok(()) => {
            bar.finish("dedup complete");
            Ok(())
        }
        Err(Error::Interrupted) if force => {
            bar.abandon();
            Err(Error::Interrupted)
        }
        Err(Error::Interrupted) => {
            bar.abandon();
            Err(Error::Interrupted)
        }
        Err(e) => Err(e),
    }
}

fn run_inner(
    config: &Config,
    db: &Database,
    shutdown: &Shutdown,
    bar: &CountProgress,
) -> Result<()> {
    for group in db.duplicate_groups()? {
        shutdown.check_between_files()?;
        dedup_group(config, db, shutdown, &group.members, bar)?;
    }

    for record in db.files_in_phase(crate::db::types::FilePhase::Hashed)? {
        shutdown.check_between_files()?;
        bar.set_file("dedup", &record.rel_path);
        let path = config.input_dir.join(&record.rel_path);
        warn_if_times_changed(&path, record.mtime, record.atime, record.ctime);
        db.mark_self_canonical(record.id)?;
        bar.inc(1);
    }

    Ok(())
}

/// Within one `(sha1, size)` bucket, partition files into equivalence classes by
/// binary comparison against one representative per class — O(n × k) compares.
fn dedup_group(
    config: &Config,
    db: &Database,
    shutdown: &Shutdown,
    members: &[FileId],
    bar: &CountProgress,
) -> Result<()> {
    let mut members = members.to_vec();
    members.sort_by_key(|id| id.0);

    // One canonical file id per distinct binary content in this hash bucket.
    let mut representatives: Vec<FileId> = Vec::new();

    for member in members {
        shutdown.check_between_files()?;

        let member_path = resolve_path(config, db, member)?;
        let rel = db
            .get_file(member)?
            .map(|r| r.rel_path)
            .unwrap_or_default();
        bar.set_file("dedup", &rel);

        let mut matched = None;
        for &rep in &representatives {
            shutdown.check_between_files()?;
            let rep_path = resolve_path(config, db, rep)?;
            if files_equal(&member_path, &rep_path, shutdown)? {
                matched = Some(rep);
                break;
            }
        }

        match matched {
            Some(rep) => db.set_canonical(member, rep)?,
            None => {
                db.mark_self_canonical(member)?;
                representatives.push(member);
            }
        }
        bar.inc(1);
    }

    Ok(())
}

fn resolve_path(config: &Config, db: &Database, file_id: FileId) -> Result<std::path::PathBuf> {
    let record = db
        .get_file(file_id)?
        .ok_or_else(|| crate::error::Error::Config(format!("missing file id {}", file_id.0)))?;
    let path = config.input_dir.join(&record.rel_path);
    warn_if_times_changed(&path, record.mtime, record.atime, record.ctime);
    Ok(path)
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

    let mut buf_a = io_buffer();
    let mut buf_b = io_buffer();
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
