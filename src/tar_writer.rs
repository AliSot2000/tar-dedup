use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;
use tar::Builder;

use crate::config::CompressionFormat;
use crate::error::{Error, Result};
use crate::shutdown::Shutdown;

pub struct TarWriter {
    archive_path: PathBuf,
    layer: CompressLayer,
    bytes_in: u64,
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

struct ShutdownWrite<'a, W> {
    inner: &'a mut W,
    shutdown: Shutdown,
}

impl<W: Write> Write for ShutdownWrite<'_, W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.shutdown
            .check_in_flight()
            .map_err(|_| io::Error::new(io::ErrorKind::Interrupted, "interrupted"))?;
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.shutdown
            .check_in_flight()
            .map_err(|_| io::Error::new(io::ErrorKind::Interrupted, "interrupted"))?;
        self.inner.flush()
    }
}

struct MeteredReader<R, F> {
    inner: R,
    shutdown: Shutdown,
    on_read: F,
}

impl<R: Read, F: FnMut(u64)> Read for MeteredReader<R, F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.shutdown
            .check_in_flight()
            .map_err(|_| io::Error::new(io::ErrorKind::Interrupted, "interrupted"))?;
        let n = self.inner.read(buf)?;
        if n > 0 {
            (self.on_read)(n as u64);
        }
        Ok(n)
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
            bytes_in: 0,
        })
    }

    pub fn append_path(
        &mut self,
        path: &Path,
        tar_name: &str,
        shutdown: &Shutdown,
        mut on_input_bytes: impl FnMut(u64),
    ) -> Result<()> {
        let mut file = File::open(path).map_err(|e| crate::error::Error::io(path, e))?;
        let len = file
            .metadata()
            .map_err(|e| crate::error::Error::io(path, e))?
            .len();

        let mut header = tar::Header::new_gnu();
        header
            .set_path(tar_name)
            .map_err(|e| crate::error::Error::Other(anyhow::anyhow!("{e}")))?;
        header.set_size(len);
        header.set_cksum();

        let mut metered = MeteredReader {
            inner: &mut file,
            shutdown: shutdown.clone(),
            on_read: |n| {
                on_input_bytes(n);
            },
        };

        {
            let mut guarded = ShutdownWrite {
                inner: &mut self.layer,
                shutdown: shutdown.clone(),
            };
            let mut builder = Builder::new(&mut guarded);
            builder
                .append(&header, &mut metered)
                .map_err(io_to_error)?;
            builder.finish().map_err(io_to_error)?;
        }

        shutdown.check_in_flight()?;
        self.layer
            .flush()
            .map_err(|e| crate::error::Error::io(&self.archive_path, e))?;
        self.bytes_in += len;
        Ok(())
    }

    pub fn finalize_session(self, shutdown: &Shutdown) -> Result<(u64, u64)> {
        shutdown.check_in_flight()?;
        let bytes_in = self.bytes_in;
        let archive_path = self.archive_path.clone();
        let bytes_out = match self.layer {
            CompressLayer::Xz(w) => {
                w.finish()
                    .map_err(|e| crate::error::Error::io(&archive_path, e))?
                    .metadata()
                    .map_err(|e| crate::error::Error::io(&archive_path, e))?
                    .len()
            }
            CompressLayer::Gz(w) => {
                w.finish()
                    .map_err(|e| crate::error::Error::io(&archive_path, e))?
                    .metadata()
                    .map_err(|e| crate::error::Error::io(&archive_path, e))?
                    .len()
            }
            CompressLayer::Bz(w) => {
                w.finish()
                    .map_err(|e| crate::error::Error::io(&archive_path, e))?
                    .metadata()
                    .map_err(|e| crate::error::Error::io(&archive_path, e))?
                    .len()
            }
            CompressLayer::Zstd(w) => {
                w.finish()
                    .map_err(|e| crate::error::Error::io(&archive_path, e))?
                    .metadata()
                    .map_err(|e| crate::error::Error::io(&archive_path, e))?
                    .len()
            }
            CompressLayer::Plain(mut w) => {
                w.flush()
                    .map_err(|e| crate::error::Error::io(&archive_path, e))?;
                w.metadata()
                    .map_err(|e| crate::error::Error::io(&archive_path, e))?
                    .len()
            }
        };
        Ok((bytes_in, bytes_out))
    }
}

fn io_to_error(e: io::Error) -> Error {
    if e.kind() == io::ErrorKind::Interrupted {
        Error::Interrupted
    } else {
        Error::Other(anyhow::anyhow!("{e}"))
    }
}
