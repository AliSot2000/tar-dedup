use crate::common::files::warn_if_times_changed;
use crate::config::ArchiveConfig;
use crate::db::flags::FileFlag;
use crate::db::types::StrippedRecord;
use crate::db::Database;
use crate::error::Result;
use path_clean::PathClean;
use std::fs;
use std::os::unix::fs::symlink;

use crate::shutdown::Shutdown;

const EXPECTED_CANONICAL: &str = "stage: Expected only canonical files. \
                            Got wrong file type or non-canonical file";

pub fn run(config: &ArchiveConfig, db: &Database, shutdown: &Shutdown) -> Result<()> {
    fs::create_dir_all(config.paths.stage_dir())
        .map_err(|e| crate::error::Error::io(&config.paths.stage_dir(), e))?;

    let promoted = db.promote_unstageable_files(config.pipeline.retry_missing_sha)?;
    tracing::info!("Promoted {promoted} entries to staged which aren't eligible");

    // TODO progressbar
    // TODO batching
    // TODO logging
    let file_vec: Vec<StrippedRecord> = db.list_files_to_stage(config.pipeline.retry_missing_sha)?;
    let total_files = file_vec.len();
    for record in file_vec {
        shutdown.check_between_files()?;
        
        // Determine the Source
        let source_path = if record.flags.get(FileFlag::HasSparse) {
            let sparse_name = record.sparse_member_name().expect(EXPECTED_CANONICAL);
            config.paths.stage_dir().join(sparse_name).clean()
        } else {
            record.abs_path.to_path_buf()
        };

        // Determine the Destination
        let tar_name = record.tar_member_name().expect(EXPECTED_CANONICAL);
        warn_if_times_changed(
            &source_path,
            record.mtime,
            record.atime,
            record.ctime,
        );
        debug_assert_eq!(source_path, source_path.clean(), "Source Paths must be normalized");
        let target = config.paths.stage_dir().join(tar_name);
        if target.exists() {
            fs::remove_file(&target).map_err(|e| crate::error::Error::io(&target, e))?;
        }
        symlink(&source_path, &target).map_err(|e| crate::error::Error::io(&target, e))?;
        db.mark_file_phase(record.id, crate::db::types::FilePhase::Staged)?;
    }
    tracing::info!("Staged {} files", total_files);
    // Live DB already lives in the flat work dir (`snapshot.sqlite`); tar-writer
    // stages a copy via `.snapshot-for-tar.sqlite` when appending to the archive.
    Ok(())
}
