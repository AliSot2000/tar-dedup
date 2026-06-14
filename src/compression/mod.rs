use std::path::Path;

use crate::config::CompressionFormat;
use crate::error::Result;

pub mod bzip;
pub mod gzip;
pub mod xz;

pub fn warn_on_start(format: CompressionFormat) {
    if format.is_single_shot() {
        eprintln!(
            "warning: gzip single-stream mode is intended as a long, non-interruptible run."
        );
    } else if format.allows_resume() {
        eprintln!(
            "warning: each pause finalizes a compression stream; repeated interrupts increase archive size."
        );
    }
}

pub fn open_append_output(path: &Path) -> Result<std::fs::File> {
    use std::fs::OpenOptions;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| crate::error::Error::io(path, e))
}
