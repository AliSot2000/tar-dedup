use std::io::{self, Write};

use lzma_sys;
use xz2::stream::{Action, Check, MtStreamBuilder, Status, Stream};

use crate::error::{Error, Result};
use crate::shutdown::Shutdown;

/// LZMA2 preset passed to liblzma (`xz -9`).
pub const PRESET: u32 = 9;
/// Return from `process` periodically so shutdown can be polled during archive.
const TIMEOUT_MS: u32 = 1000;
/// `0` = liblzma default block size (3× dict or 1 MiB); matches `xz -9 -T16`.
const BLOCK_SIZE: u64 = 0;

pub struct InterruptibleXzEncoder<W: Write> {
    stream: Stream,
    obj: Option<W>,
    buf: Vec<u8>,
    shutdown: Shutdown,
}

/// Approximate RAM for multithreaded xz encoder (same API `xz -vv` uses).
pub fn mt_memusage(threads: u32) -> u64 {
    let mut builder = MtStreamBuilder::new();
    builder
        .threads(threads.max(1))
        .preset(PRESET)
        .timeout_ms(TIMEOUT_MS)
        .block_size(BLOCK_SIZE)
        .check(Check::Crc64);
    builder.memusage()
}

/// Pick thread count, optionally clamping to `--memlimit-compress` like the xz CLI.
pub fn resolve_xz_threads(requested: usize, memlimit: Option<u64>) -> Result<u32> {
    let requested = requested.max(1) as u32;
    let mut threads = requested;
    let mut memusage = mt_memusage(threads);

    eprintln!(
        "xz: {} of memory is required for preset -{} with {} thread(s).",
        format_mib(memusage),
        PRESET,
        threads
    );

    if let Some(limit) = memlimit {
        eprintln!("xz: the memory limit is {}.", format_mib(limit));
        while memusage > limit && threads > 1 {
            threads -= 1;
            memusage = mt_memusage(threads);
        }
        if memusage > limit {
            return Err(Error::Config(format!(
                "xz preset -{PRESET} needs at least {} ({} thread(s)); \
                 raise --memlimit-compress or lower --jobs",
                format_mib(memusage),
                threads
            )));
        }
        if threads < requested {
            eprintln!(
                "xz: reduced threads from {requested} to {threads} to stay within the {} limit",
                format_mib(limit)
            );
        }
    } else {
        eprintln!("xz: memory limiter disabled (use --memlimit-compress to cap usage).");
        if let Some(ram) = physical_ram_bytes() {
            if memusage > ram {
                eprintln!(
                    "xz: warning: estimated need ({}) exceeds physical RAM ({})",
                    format_mib(memusage),
                    format_mib(ram)
                );
            }
        }
    }

    tracing::info!(
        threads,
        block_size = BLOCK_SIZE,
        memusage,
        memlimit = ?memlimit,
        "xz multithreaded encoder"
    );

    Ok(threads)
}

pub fn format_mib(bytes: u64) -> String {
    let mib = (bytes + (1024 * 1024 - 1)) / (1024 * 1024);
    // Thin space grouping like xz (ASCII apostrophe fallback).
    let s = mib.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push('\'');
        }
        out.push(ch);
    }
    format!("{} MiB", out.chars().rev().collect::<String>())
}

fn physical_ram_bytes() -> Option<u64> {
    let line = std::fs::read_to_string("/proc/meminfo")
        .ok()?
        .lines()
        .find(|l| l.starts_with("MemTotal:"))?
        .to_string();
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

impl<W: Write> InterruptibleXzEncoder<W> {
    pub fn new(
        file: W,
        jobs: usize,
        memlimit: Option<u64>,
        shutdown: Shutdown,
    ) -> Result<(Self, u32)> {
        let threads = resolve_xz_threads(jobs, memlimit)?;

        let mut builder = MtStreamBuilder::new();
        builder
            .threads(threads)
            .preset(PRESET)
            .timeout_ms(TIMEOUT_MS)
            .block_size(BLOCK_SIZE)
            .check(Check::Crc64);

        let stream = builder.encoder().map_err(|e| {
            Error::Other(anyhow::anyhow!("xz multithreaded encoder ({threads} threads): {e}"))
        })?;

        Ok((
            Self {
                stream,
                obj: Some(file),
                buf: Vec::with_capacity(32 * 1024),
                shutdown,
            },
            threads,
        ))
    }

    pub fn hardware_threads() -> u32 {
        unsafe { lzma_sys::lzma_cputhreads() }
    }

    fn dump(&mut self) -> io::Result<()> {
        while !self.buf.is_empty() {
            let n = self.obj.as_mut().unwrap().write(&self.buf)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "xz output stalled"));
            }
            self.buf.drain(..n);
        }
        Ok(())
    }

    pub fn try_finish(&mut self) -> io::Result<()> {
        loop {
            self.shutdown
                .check_in_flight()
                .map_err(|_| io::Error::new(io::ErrorKind::Interrupted, "interrupted"))?;
            self.dump()?;
            let status = self
                .stream
                .process_vec(&[], &mut self.buf, Action::Finish)
                .map_err(|e| io::Error::other(format!("xz finish: {e}")))?;
            if status == Status::StreamEnd {
                break;
            }
        }
        self.dump()
    }

    pub fn finish(mut self) -> io::Result<W> {
        self.try_finish()?;
        Ok(self.obj.take().unwrap())
    }

    /// Drop the encoder without writing the stream footer (force-abort).
    pub fn abandon(mut self) -> W {
        self.obj.take().unwrap()
    }
}

impl<W: Write> Write for InterruptibleXzEncoder<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if data.is_empty() {
            return Ok(0);
        }

        let mut input_off = 0usize;
        while input_off < data.len() {
            self.shutdown
                .check_in_flight()
                .map_err(|_| io::Error::new(io::ErrorKind::Interrupted, "interrupted"))?;
            self.dump()?;

            let before = self.stream.total_in();
            self.stream
                .process_vec(&data[input_off..], &mut self.buf, Action::Run)
                .map_err(|e| io::Error::other(format!("xz compress: {e}")))?;
            let consumed = (self.stream.total_in() - before) as usize;

            if consumed > 0 {
                input_off += consumed;
            } else {
                std::thread::yield_now();
            }
        }

        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        // Streaming: do not FullFlush between tar entries — that would sync all MT
        // workers and break the LZMA2 dictionary (unlike tar | xz). Session close
        // uses finish() only (graceful stop or normal completion).
        self.dump()?;
        self.obj.as_mut().unwrap().flush()
    }
}
