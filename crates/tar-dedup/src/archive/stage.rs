use std::fs;
use std::os::unix::fs::symlink;

use crate::common::files::warn_if_times_changed;
use crate::config::Config;
use crate::db::content_id::content_id_from_digest;
use crate::db::Database;
use crate::error::Result;

use crate::shutdown::Shutdown;

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    std::fs::create_dir_all(config.stage_dir())
        .map_err(|e| crate::error::Error::io(&config.stage_dir(), e))?;

    for file_id in db.list_canonical_files(crate::db::types::FilePhase::Deduped)? {
        shutdown.check_between_files()?;
        let Some(record) = db.get_file(file_id)? else {
            continue;
        };
        let digest = record.sha1.ok_or_else(|| {
            crate::error::Error::Config(format!("canonical {} missing sha1", file_id.0))
        })?;
        let content_id =
            content_id_from_digest(&digest, record.size, file_id, &record.rel_path);
        let tar_name = content_id.0.as_str();
        let source_rel = config.input_dir.join(&record.rel_path);
        warn_if_times_changed(
            &source_rel,
            record.mtime,
            record.atime,
            record.ctime,
        );
        let source = source_rel
            .canonicalize()
            .map_err(|e| crate::error::Error::io(&source_rel, e))?;
        let target = config.stage_dir().join(tar_name);
        if target.exists() {
            fs::remove_file(&target).map_err(|e| crate::error::Error::io(&target, e))?;
        }
        symlink(&source, &target).map_err(|e| crate::error::Error::io(&target, e))?;
        db.set_tar_path(file_id, tar_name)?;
        db.mark_file_phase(file_id, crate::db::types::FilePhase::Staged)?;
    }

    copy_database(config)?;
    Ok(())
}

fn copy_database(config: &Config) -> Result<()> {
    let src = config.db_path();
    let dst = config.stage_dir().join("snapshot.sqlite");
    std::fs::copy(&src, &dst).map_err(|e| crate::error::Error::io(&dst, e))?;
    Ok(())
}
