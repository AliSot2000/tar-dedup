use crate::config::Config;
use crate::db::Database;
use crate::error::Result;
use crate::shutdown::Shutdown;
use crate::tar_writer::TarWriter;

pub fn run(config: &Config, db: &Database, shutdown: &Shutdown) -> Result<()> {
    if shutdown.requested() {
        return Ok(());
    }

    let session_id = match db.open_archive_session()? {
        Some(open) => open.id,
        None => db.begin_archive_session()?,
    };

    let mut writer = TarWriter::open(config.archive_path.clone(), config.compression, session_id)?;

    for file_id in db.list_canonical_files()? {
        if shutdown.requested() {
            break;
        }
        let Some(record) = db.get_file(file_id)? else {
            continue;
        };
        let digest = record.sha1.expect("canonical hashed");
        let tar_name = crate::content_id::content_id_from_digest(&digest, record.size).0;
        let source = config.stage_dir().join(&tar_name);
        writer.append_path(&source, &tar_name)?;
        db.queue_archive_entry(file_id, session_id, &tar_name)?;
    }

    let (bytes_in, bytes_out) = writer.finalize_session()?;
    db.finalize_archive_session(session_id, bytes_in, bytes_out)?;
    Ok(())
}
