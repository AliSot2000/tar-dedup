use std::io::Write;

pub fn encoder<'a>(writer: &'a mut dyn Write) -> crate::error::Result<Box<dyn Write + 'a>> {
    Ok(Box::new(xz2::write::XzEncoder::new(writer, 9)))
}
