use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;
use tar::Header;

use crate::compression::InterruptibleXzEncoder;
use crate::config::CompressionFormat;
use crate::error::{Error, Result};
use crate::progress::io_buffer;
use crate::shutdown::Shutdown;

pub struct TarWriter {
    archive_path: PathBuf,
    layer: CompressLayer,
    bytes_in: u64,
}

enum CompressLayer {
    Xz(InterruptibleXzEncoder<File>),
    Gz(GzEncoder<File>),
    Bz(bzip2::write::BzEncoder<File>),
    Zstd(zstd::stream::write::Encoder<'static, File>),
    Plain(File),
}

impl Write for CompressLayer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Xz(w) => w.write(buf),
            Self::Gz(w) => w.write(buf),
            Self::Bz(w) => w.write(buf),
            Self::Zstd(w) => w.write(buf),
            Self::Plain(w) => w.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
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

impl TarWriter {
    pub fn open(
        archive_path: PathBuf,
        format: CompressionFormat,
        jobs: usize,
        shutdown: Shutdown,
    ) -> Result<Self> {
        crate::compression::warn_on_start(format);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&archive_path)
            .map_err(|e| crate::error::Error::io(&archive_path, e))?;

        let layer = match format {
            CompressionFormat::Xz => {
                CompressLayer::Xz(InterruptibleXzEncoder::new(file, jobs, shutdown)?)
            }
            CompressionFormat::Gz => {
                CompressLayer::Gz(GzEncoder::new(file, Compression::best()))
            }
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

        let mut header = Header::new_gnu();
        header
            .set_path(tar_name)
            .map_err(|e| crate::error::Error::Other(anyhow::anyhow!("{e}")))?;
        header.set_size(len);
        header.set_cksum();

        let mut out = ShutdownWrite {
            inner: &mut self.layer,
            shutdown: shutdown.clone(),
        };

        out.write_all(header.as_bytes())
            .map_err(|e| io_to_error(e, &self.archive_path))?;

        let mut buf = io_buffer();
        let mut remaining = len;
        while remaining > 0 {
            shutdown.check_in_flight()?;
            let chunk = std::cmp::min(buf.len() as u64, remaining) as usize;
            let n = file
                .read(&mut buf[..chunk])
                .map_err(|e| crate::error::Error::io(path, e))?;
            if n == 0 {
                return Err(crate::error::Error::Other(anyhow::anyhow!(
                    "unexpected EOF reading {}",
                    path.display()
                )));
            }
            out.write_all(&buf[..n])
                .map_err(|e| io_to_error(e, &self.archive_path))?;
            on_input_bytes(n as u64);
            remaining -= n as u64;
        }

        let pad = (512 - (len % 512)) % 512;
        if pad > 0 {
            shutdown.check_in_flight()?;
            out.write_all(&vec![0u8; pad as usize])
                .map_err(|e| io_to_error(e, &self.archive_path))?;
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

    /// Force-abort: release the output file without finalizing compression.
    pub fn abandon(self) {
        match self.layer {
            CompressLayer::Xz(w) => {
                w.abandon();
            }
            CompressLayer::Gz(w) => {
                std::mem::forget(w);
            }
            CompressLayer::Bz(w) => {
                std::mem::forget(w);
            }
            CompressLayer::Zstd(w) => {
                std::mem::forget(w);
            }
            CompressLayer::Plain(_) => {}
        }
    }
}

fn io_to_error(e: io::Error, path: &Path) -> Error {
    if e.kind() == io::ErrorKind::Interrupted {
        Error::Interrupted
    } else {
        Error::io(path, e)
    }
}
