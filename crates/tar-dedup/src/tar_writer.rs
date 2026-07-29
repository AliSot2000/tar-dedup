use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;
use tar::Builder;

use crate::compression::InterruptibleXzEncoder;
use crate::config::CompressionFormat;
use crate::error::{Error, Result};
use crate::shutdown::Shutdown;

/// Two 512-byte zero blocks — ustar end-of-archive marker.
const TAR_EOF_LEN: usize = 1024;

/// Hold back trailing bytes so we can drop Builder's tar EOF without sniffing
/// individual `write` shapes.
///
/// `tar::Builder::{finish,into_inner,Drop}` always emits EOF; we cannot skip that
/// call. Instead we keep the last [`TAR_EOF_LEN`] bytes uncommitted. Anything older
/// is flushed as real tar payload (so a final member that ends in zeros is not lost
/// when EOF is appended). After Builder teardown, [`TarSink::resolve_trailing_eof`]
/// either commits the hold (final session) or drops it if it is an all-zero trailer
/// (graceful multi-session).
struct TarSink {
    inner: BufferedCompress,
    pending: Vec<u8>,
    allow_tar_eof: bool,
}

impl TarSink {
    fn drain_pending_excess(&mut self) -> io::Result<()> {
        if self.pending.len() <= TAR_EOF_LEN {
            return Ok(());
        }
        let release = self.pending.len() - TAR_EOF_LEN;
        self.inner.write_all(&self.pending[..release])?;
        self.pending.drain(..release);
        Ok(())
    }

    /// After `Builder::into_inner` (EOF already written into `pending`): commit or drop.
    fn resolve_trailing_eof(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let drop_eof = !self.allow_tar_eof
            && self.pending.len() <= TAR_EOF_LEN
            && self.pending.iter().all(|&b| b == 0);
        if drop_eof {
            self.pending.clear();
        } else {
            self.inner.write_all(&self.pending)?;
            self.pending.clear();
        }
        Ok(())
    }
}

impl Write for TarSink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        self.pending.extend_from_slice(data);
        self.drain_pending_excess()?;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Do not flush `pending` — it may still be / become the EOF trailer.
        self.inner.flush()
    }
}

/// Intermediate buffer between tar framing and the compressor (~4 MiB).
struct BufferedCompress {
    buf: Vec<u8>,
    layer: CompressLayer,
    shutdown: Shutdown,
}

impl BufferedCompress {
    fn new(layer: CompressLayer, shutdown: Shutdown) -> Self {
        const BUF: usize = 4 * 1024 * 1024;
        Self {
            buf: Vec::with_capacity(BUF),
            layer,
            shutdown,
        }
    }

    fn capacity(&self) -> usize {
        4 * 1024 * 1024
    }

    fn flush_buf(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let mut offset = 0;
        while offset < self.buf.len() {
            self.shutdown
                .check_in_flight()
                .map_err(|_| io::Error::new(io::ErrorKind::Interrupted, "interrupted"))?;
            let n = self.layer.write(&self.buf[offset..])?;
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "compress output stalled",
                ));
            }
            offset += n;
        }
        self.buf.clear();
        Ok(())
    }

    fn into_layer(mut self) -> io::Result<CompressLayer> {
        self.flush_buf()?;
        self.layer.flush()?;
        Ok(self.layer)
    }
}

impl Write for BufferedCompress {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }
        let mut input = data;
        while !input.is_empty() {
            self.shutdown
                .check_in_flight()
                .map_err(|_| io::Error::new(io::ErrorKind::Interrupted, "interrupted"))?;
            let space = self.capacity().saturating_sub(self.buf.len());
            if space == 0 {
                self.flush_buf()?;
                continue;
            }
            let n = space.min(input.len());
            self.buf.extend_from_slice(&input[..n]);
            input = &input[n..];
            if self.buf.len() >= self.capacity() {
                self.flush_buf()?;
            }
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buf()?;
        self.layer.flush()
    }
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

impl CompressLayer
{
    fn finish(self) -> io::Result<File> {
        match self {
            Self::Xz(w) => w.finish(),
            Self::Gz(w) => w.finish(),
            Self::Bz(w) => w.finish(),
            Self::Zstd(w) => w.finish(),
            Self::Plain(w) => Ok(w),
        }
    }
}

pub struct TarWriter {
    archive_path: PathBuf,
    builder: Option<Builder<TarSink>>,
    bytes_in: u64,
}

impl TarWriter {
    pub fn open(
        archive_path: PathBuf,
        format: CompressionFormat,
        jobs: usize,
        memlimit_compress: Option<u64>,
        shutdown: Shutdown,
    ) -> Result<Self> {
        crate::compression::warn_on_start(format);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&archive_path)
            .map_err(|e| Error::io(&archive_path, e))?;

        let layer = match format {
            CompressionFormat::Xz => {
                let (encoder, threads) =
                    InterruptibleXzEncoder::new(file, jobs, memlimit_compress, shutdown.clone())?;
                let hw = InterruptibleXzEncoder::<File>::hardware_threads();
                eprintln!(
                    "xz encoder: {threads} worker thread(s) active ({hw} CPU threads available)"
                );
                CompressLayer::Xz(encoder)
            }
            CompressionFormat::Gz => {
                CompressLayer::Gz(GzEncoder::new(file, Compression::best()))
            }
            CompressionFormat::Bz2 => {
                CompressLayer::Bz(bzip2::write::BzEncoder::new(file, bzip2::Compression::best()))
            }
            CompressionFormat::Zstd => CompressLayer::Zstd(
                zstd::stream::write::Encoder::new(file, 19)
                    .map_err(|e| Error::Other(anyhow::anyhow!("zstd encoder: {e}")))?,
            ),
            CompressionFormat::None => CompressLayer::Plain(file),
        };

        let sink = TarSink {
            inner: BufferedCompress::new(layer, shutdown),
            pending: Vec::with_capacity(TAR_EOF_LEN),
            allow_tar_eof: false,
        };
        let mut builder = Builder::new(sink);
        // Stage entries are content-id symlinks; pack the target file bytes.
        builder.follow_symlinks(true);

        Ok(Self {
            archive_path,
            builder: Some(builder),
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
        shutdown.check_in_flight()?;
        let meta = std::fs::metadata(path).map_err(|e| Error::io(path, e))?;
        let len = meta.len();

        let builder = self.builder.as_mut().expect("tar builder active");
        // Builder writes through TarSink → buffer → xz; progress is approximate by size.
        builder
            .append_path_with_name(path, tar_name)
            .map_err(|e| Error::io(&self.archive_path, e))?;

        on_input_bytes(len);
        self.bytes_in += len;
        Ok(())
    }

    /// Graceful session end: flush tar (no EOF), finish compression stream.
    pub fn finalize_session(mut self, shutdown: &Shutdown) -> Result<(u64, u64)> {
        shutdown.check_in_flight()?;
        let bytes_in = self.bytes_in;
        let archive_path = self.archive_path.clone();

        let mut sink = self.take_sink(false)?;
        sink.resolve_trailing_eof()
            .map_err(|e| Error::io(&archive_path, e))?;
        sink.flush().map_err(|e| Error::io(&archive_path, e))?;
        let file = sink
            .inner
            .into_layer()
            .map_err(|e| Error::io(&archive_path, e))?
            .finish()
            .map_err(|e| Error::io(&archive_path, e))?;
        let bytes_out = file
            .metadata()
            .map_err(|e| Error::io(&archive_path, e))?
            .len();
        Ok((bytes_in, bytes_out))
    }

    /// Final archive close: emit tar EOF, then finish compression.
    pub fn finalize_archive(mut self, shutdown: &Shutdown) -> Result<(u64, u64)> {
        shutdown.check_in_flight()?;
        let bytes_in = self.bytes_in;
        let archive_path = self.archive_path.clone();

        let mut sink = self.take_sink(true)?;
        sink.resolve_trailing_eof()
            .map_err(|e| Error::io(&archive_path, e))?;
        sink.flush().map_err(|e| Error::io(&archive_path, e))?;
        let file = sink
            .inner
            .into_layer()
            .map_err(|e| Error::io(&archive_path, e))?
            .finish()
            .map_err(|e| Error::io(&archive_path, e))?;
        let bytes_out = file
            .metadata()
            .map_err(|e| Error::io(&archive_path, e))?
            .len();
        Ok((bytes_in, bytes_out))
    }

    /// Force-abort: drop incomplete compression stream (no footer). Recovery truncates.
    pub fn abandon(mut self) {
        if let Ok(mut sink) = self.take_sink(false) {
            // Session will be truncated; drop held trailer / last block without compressing further.
            sink.pending.clear();
            match sink.inner.into_layer() {
                Ok(CompressLayer::Xz(w)) => {
                    w.abandon();
                }
                Ok(CompressLayer::Gz(w)) => {
                    std::mem::forget(w);
                }
                Ok(CompressLayer::Bz(w)) => {
                    std::mem::forget(w);
                }
                Ok(CompressLayer::Zstd(w)) => {
                    std::mem::forget(w);
                }
                Ok(CompressLayer::Plain(_)) | Err(_) => {}
            }
        }
    }

    fn take_sink(&mut self, allow_tar_eof: bool) -> Result<TarSink> {
        let mut builder = self
            .builder
            .take()
            .ok_or_else(|| Error::Config("tar builder already consumed".into()))?;
        builder.get_mut().allow_tar_eof = allow_tar_eof;
        // `into_inner` always calls `finish` → writes EOF into the sink hold-back.
        builder
            .into_inner()
            .map_err(|e| Error::io(&self.archive_path, e))
    }
}
