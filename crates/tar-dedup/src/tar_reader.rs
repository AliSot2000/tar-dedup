use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use xz2::read::XzDecoder;

use crate::config::CompressionFormat;
use crate::error::{Error, Result};

/// Decompressed byte stream for a tar-dedup archive (handles concatenated xz/gzip streams).
pub fn open_decompressed(path: &Path, format: CompressionFormat) -> Result<Box<dyn Read>> {
    let file = File::open(path).map_err(|e| Error::io(path, e))?;
    let reader: Box<dyn Read> = match format {
        CompressionFormat::Xz => Box::new(XzDecoder::new(file)),
        CompressionFormat::Gz => Box::new(GzDecoder::new(file)),
        CompressionFormat::Bz2 => Box::new(BzDecoder::new(file)),
        CompressionFormat::Zstd => Box::new(
            zstd::stream::read::Decoder::new(file)
                .map_err(|e| Error::Other(anyhow::anyhow!("zstd decoder: {e}")))?,
        ),
        CompressionFormat::PIPE => return Err(Error::Other(anyhow::anyhow!("Pipe not Implemented"))),
        CompressionFormat::None => Box::new(file),
    };
    Ok(reader)
}

pub fn open_tar_archive(path: &Path, format: CompressionFormat) -> Result<tar::Archive<BufReader<Box<dyn Read>>>> {
    let raw = open_decompressed(path, format)?;
    Ok(tar::Archive::new(BufReader::with_capacity(1024 * 1024, raw)))
}
