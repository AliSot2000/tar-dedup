use chrono::{DateTime, Utc};

use std::io;
use std::path::Path;

/// Get all times associated with the file. Result is `(mtime, atime, ctime)`
pub fn get_file_times(meta: &std::fs::Metadata)
    -> (io::Result<DateTime<Utc>>,
                   io::Result<DateTime<Utc>>,
                   io::Result<DateTime<Utc>>) {
    let file_mtime = file_mtime(&meta);
    let file_atime = file_atime(&meta);
    let file_ctime = file_ctime(&meta);
    (file_mtime, file_atime, file_ctime)

}

fn file_mtime(meta: &std::fs::Metadata) -> io::Result<DateTime<Utc>> {
    Ok(DateTime::<Utc>::from(meta.modified()?))
}

fn file_atime(meta: &std::fs::Metadata) -> io::Result<DateTime<Utc>> {
    Ok(DateTime::<Utc>::from(meta.accessed()?))
}

fn file_ctime(meta: &std::fs::Metadata) -> io::Result<DateTime<Utc>> {
    Ok(DateTime::<Utc>::from(meta.created()?))
}
