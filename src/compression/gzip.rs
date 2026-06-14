use std::io::Write;

use flate2::write::GzEncoder;
use flate2::Compression;

use crate::error::Result;

pub fn encoder<'a>(writer: &'a mut dyn Write) -> Result<Box<dyn Write + 'a>> {
    Ok(Box::new(GzEncoder::new(
        writer,
        Compression::best(),
    )))
}
