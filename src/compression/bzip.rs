use std::io::Write;

use crate::error::Result;

pub fn encoder<'a>(writer: &'a mut dyn Write) -> Result<Box<dyn Write + 'a>> {
    Ok(Box::new(bzip2::write::BzEncoder::new(
        writer,
        bzip2::Compression::best(),
    )))
}
