use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use flate2::write::GzEncoder;
use flate2::Compression;
use tar::Builder;

use crate::compression::InterruptibleXzEncoder;
use crate::config::{CompressionFormat, CompressionSettings};
use crate::error::{Error, Result};
use crate::shutdown::Shutdown;

/// Two 512-byte zero blocks — ustar end-of-archive marker.
const TAR_EOF_LEN: usize = 1024;

pub struct TarWriter {
    archive_path: PathBuf,
    /// Needed to have option for solid tear-down ownership.
    builder: Option<Builder<TarSink>>,
    bytes_in: u64,
}

/// # Tar writer pipeline:
/// - `TarWriter` writes into
/// - `Builder` writes into
/// - `TarSink` writes into
/// - `BufferedCompress` writes into
/// - `CompressionLayer` (XZ, Gzip, Zlib Bzip2, None) writes into
/// - `File`
impl TarWriter {
    /// Open a new TarWriter (and other buffer objects)
    pub fn open(
        archive_path: PathBuf,
        settings: &CompressionSettings,
        jobs: usize,
        shutdown: Shutdown,
    ) -> Result<Self> {
        let format = settings.format;
        let compress_level = settings.level;
        let xz_extreme = settings.xz_extreme;
        let bzip_small = settings.bzip_small;
        let memlimit_compress = settings.memlimit_compress;

        // CLI already validated level / format-specific flags.
        debug_assert!(
            match format.level_range() {
                None => true,
                Some((min, max)) => compress_level >= min && compress_level <= max,
            },
            "compress_level {compress_level} out of range for {format:?}"
        );
        debug_assert!(
            !xz_extreme || matches!(format, CompressionFormat::Xz),
            "xz_extreme set for non-xz format"
        );
        debug_assert!(
            !bzip_small || matches!(format, CompressionFormat::Bz2),
            "bzip_small set for non-bzip2 format"
        );
        debug_assert!(
            !bzip_small || compress_level == 1,
            "bzip_small requires level 1"
        );

        crate::compression::warn_on_start(format);

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&archive_path)
            .map_err(|e| Error::io(&archive_path, e))?;

        let layer = match format {
            CompressionFormat::Xz => {
                let mut preset = compress_level;
                if xz_extreme {
                    preset |= 1u32 << 31; // LZMA_PRESET_EXTREME
                }
                let (encoder, threads) = InterruptibleXzEncoder::new(
                    file,
                    jobs,
                    memlimit_compress,
                    preset,
                    shutdown.clone(),
                )?;
                let hw = InterruptibleXzEncoder::<File>::hardware_threads();
                eprintln!(
                    "xz encoder: {threads} worker thread(s) active ({hw} CPU threads available)"
                );
                CompressLayer::Xz(encoder)
            }
            CompressionFormat::Gz => {
                CompressLayer::Gz(GzEncoder::new(file, Compression::new(compress_level)))
            }
            CompressionFormat::Bz2 => {
                CompressLayer::Bz(bzip2::write::BzEncoder::new(
                    file,
                    bzip2::Compression::new(compress_level),
                ))
            }
            CompressionFormat::Zstd => CompressLayer::Zstd(
                zstd::stream::write::Encoder::new(file, compress_level as i32)
                    .map_err(|e| Error::Other(anyhow::anyhow!("zstd encoder: {e}")))?,
            ),
            CompressionFormat::None => CompressLayer::Plain(file),
        };

        let sink = TarSink {
            inner: BufferedCompress::new(layer, shutdown),
            pending: Vec::with_capacity(TAR_EOF_LEN),
            allow_tar_eof: false,
        };
        // TODO: sparse
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
        let error_factory = |e| Error::io(&archive_path, e);
        let err_fac = error_factory; // Info: Alias for convenience.

        let mut sink = self.take_sink(false)?;

        sink
            .resolve_trailing_eof()
            .map_err(err_fac)?;
        sink
            .flush()
            .map_err(err_fac)?;

        let bytes_out = sink
            .inner
            .into_layer()
            .map_err(err_fac)?
            .finish()
            .map_err(err_fac)?
            .metadata()
            .map_err(err_fac)?
            .len();
        Ok((bytes_in, bytes_out))
    }

    /// Final archive close: emit tar EOF, then finish compression.
    pub fn finalize_archive(mut self, shutdown: &Shutdown) -> Result<(u64, u64)> {
        shutdown.check_in_flight()?;

        let bytes_in = self.bytes_in;
        let archive_path = self.archive_path.clone();
        let error_factory = |e| Error::io(&archive_path, e);
        let err_fac = error_factory; // Info: Alias for convenience.

        let mut sink = self.take_sink(true)?;
        sink
            .resolve_trailing_eof()
            .map_err(err_fac)?;
        sink
            .flush()
            .map_err(err_fac)?;

        let bytes_out = sink
            .inner
            .into_layer()
            .map_err(err_fac)?
            .finish()
            .map_err(err_fac)?
            .metadata()
            .map_err(err_fac)?
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

    /// Remove Some(Builder) from the struct and then returns the inner TarSink
    /// Hint, it is needed that the Object that is written into is owned by the Builder.
    fn take_sink(&mut self, allow_tar_eof: bool) -> Result<TarSink> {
        // Function removes Some(Builder)
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
/// We use this to ensure that we can have enough checks for SIGINT.
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
        // Loop should be technically not be executed multiple times. Technically only once.
        // Used to deal with the compression layer not accepting everything
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
        // Loop is used to consume the full buffer in batches and then propagate into
        // the compression buffer.
        while !input.is_empty() {
            self.shutdown
                .check_in_flight()
                // TODO better error capture.
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
