use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use flate2::write::GzEncoder;
use flate2::Compression;
use tar::Builder;

use crate::config::CompressionFormat;
use crate::error::Result;

pub struct TarWriter {
    archive_path: PathBuf,
    layer: CompressLayer,
}

enum CompressLayer {
    Xz(xz2::write::XzEncoder<File>),
    Gz(GzEncoder<File>),
    Bz(bzip2::write::BzEncoder<File>),
    Zstd(zstd::stream::write::Encoder<'static, File>),
    Plain(File),
}

impl Write for CompressLayer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Xz(w) => w.write(buf),
            Self::Gz(w) => w.write(buf),
            Self::Bz(w) => w.write(buf),
            Self::Zstd(w) => w.write(buf),
            Self::Plain(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Xz(w) => w.flush(),
            Self::Gz(w) => w.flush(),
            Self::Bz(w) => w.flush(),
            Self::Zstd(w) => w.flush(),
            Self::Plain(w) => w.flush(),
        }
    }
}

impl TarWriter {
    pub fn open(archive_path: PathBuf, format: CompressionFormat, _session_id: i64) -> Result<Self> {
        crate::compression::warn_on_start(format);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&archive_path)
            .map_err(|e| crate::error::Error::io(&archive_path, e))?;

        let layer = match format {
            CompressionFormat::Xz => CompressLayer::Xz(xz2::write::XzEncoder::new(file, 9)),
            CompressionFormat::Gz => CompressLayer::Gz(GzEncoder::new(file, Compression::best())),
            CompressionFormat::Bz2 => {
                CompressLayer::Bz(bzip2::write::BzEncoder::new(file, bzip2::Compression::best()))
            }
            CompressionFormat::Zstd => CompressLayer::Zstd(
                zstd::stream::write::Encoder::new(file, 19)
                    .map_err(|e| crate::error::Error::Other(anyhow::anyhow!("zstd encoder: {e}")))?,
            ),
            CompressionFormat::None => CompressLayer::Plain(file),
        };

        Ok(Self {
            archive_path,
            layer,
        })
    }

    pub fn append_path(&mut self, path: &std::path::Path, tar_name: &str) -> Result<()> {
        let mut file = File::open(path).map_err(|e| crate::error::Error::io(path, e))?;
        let len = file.metadata().map_err(|e| crate::error::Error::io(path, e))?.len();

        let mut header = tar::Header::new_gnu();
        header.set_path(tar_name).map_err(|e| crate::error::Error::Other(anyhow::anyhow!("{e}")))?;
        header.set_size(len);
        header.set_cksum();

        {
            let mut builder = Builder::new(&mut self.layer);
            builder.append(&header, &mut file).map_err(|e| crate::error::Error::Other(anyhow::anyhow!("{e}")))?;
            builder.finish().map_err(|e| crate::error::Error::Other(anyhow::anyhow!("{e}")))?;
        }
        self.layer.flush().map_err(|e| crate::error::Error::io(&self.archive_path, e))?;
        Ok(())
    }

    pub fn finalize_session(self) -> Result<(u64, u64)> {
        match self.layer {
            CompressLayer::Xz(w) => {
                w.finish().map_err(|e| crate::error::Error::io(&self.archive_path, e))?;
            }
            CompressLayer::Gz(w) => {
                w.finish().map_err(|e| crate::error::Error::io(&self.archive_path, e))?;
            }
            CompressLayer::Bz(w) => {
                w.finish().map_err(|e| crate::error::Error::io(&self.archive_path, e))?;
            }
            CompressLayer::Zstd(w) => {
                w.finish().map_err(|e| crate::error::Error::io(&self.archive_path, e))?;
            }
            CompressLayer::Plain(mut w) => {
                w.flush().map_err(|e| crate::error::Error::io(&self.archive_path, e))?;
            }
        }
        Ok((0, 0))
    }
}
