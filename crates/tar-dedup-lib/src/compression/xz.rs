use std::io::{self, Read, Write};

use lzma_sys;
use xz2::read::XzDecoder;
use xz2::stream::{Action, Check, MtStreamBuilder, Status, Stream};
use xz2::write::XzEncoder;

use crate::error::{Error, Result};
use crate::shutdown::Shutdown;

/// LZMA2 preset level bits (`xz -0`…`-9`). Extreme is ORed in by callers.
pub const PRESET_LEVEL_MASK: u32 = lzma_sys::LZMA_PRESET_LEVEL_MASK;
/// `xz --extreme` / `-e`.
pub const PRESET_EXTREME: u32 = lzma_sys::LZMA_PRESET_EXTREME;
/// Fixed xz preset for the archive footer sqlite blob (`xz -9 -e`, liblzma maximum).
pub const FOOTER_XZ_PRESET: u32 = 9 | PRESET_EXTREME;
/// Return from `process` periodically so shutdown can be polled during archive.
const TIMEOUT_MS: u32 = 1000;
/// `0` = liblzma default block size (3× dict or 1 MiB); matches `xz -9 -T16`.
const BLOCK_SIZE: u64 = 0;

/// Approximate RAM for multithreaded xz encoder (same API `xz -vv` uses).
pub fn mt_memusage(threads: u32, preset: u32) -> u64 {
    let mut builder = MtStreamBuilder::new();
    builder
        .threads(threads.max(1))
        .preset(preset)
        .timeout_ms(TIMEOUT_MS)
        .block_size(BLOCK_SIZE)
        .check(Check::Crc64);
    builder.memusage()
}

/// Pick thread count, optionally clamping to `--memlimit-compress` like the xz CLI.
pub fn resolve_xz_threads(requested: usize, memlimit: Option<u64>, preset: u32) -> Result<u32> {
    let level = preset & PRESET_LEVEL_MASK;
    let requested = requested.max(1) as u32;
    let mut threads = requested;
    let mut memusage = mt_memusage(threads, preset);
    let mem_limit_str = match memlimit {
        Some(m) => format!(" and {} memorsy limit", format_mib(m)),
        None => "".into(),
    };

    tracing::info!(
        "xz: {} of memory is required for preset -{level}{} with {} thread(s){}.",
        format_mib(memusage),
        if preset & PRESET_EXTREME != 0 { "e" } else { "" },
        threads,
        mem_limit_str
    );

    if let Some(limit) = memlimit {
        tracing::info!("xz: the memory limit is {}.", format_mib(limit));
        while memusage > limit && threads > 1 {
            threads -= 1;
            memusage = mt_memusage(threads, preset);
        }
        if memusage > limit {
            return Err(Error::Config(format!(
                "xz: preset -{level}{} needs at least {} ({} thread(s)); \
                 raise --memlimit-compress or lower --jobs",
                if preset & PRESET_EXTREME != 0 { "e" } else { "" },
                format_mib(memusage),
                threads
            )));
        }
        if threads < requested {
            tracing::warn!(
                "xz: reduced threads from {requested} to {threads} to stay within the {} limit",
                format_mib(limit)
            );
        }
    } else {
        if let Some(ram) = physical_ram_bytes() {
            if memusage > ram {
                tracing::warn!(
                    "xz: estimated need ({}) exceeds physical RAM ({})",
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
        preset,
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

/// Compress bytes for the archive footer (single-threaded `xz -10 -e`, CRC64).
pub fn compress_footer_bytes(input: &[u8]) -> Result<Vec<u8>> {
    let stream = Stream::new_easy_encoder(FOOTER_XZ_PRESET, Check::Crc64)
        .map_err(|e| Error::Other(anyhow::anyhow!("xz footer encoder: {e}")))?;
    let mut out = Vec::new();
    let mut enc = XzEncoder::new_stream(&mut out, stream);
    enc.write_all(input)
        .map_err(|e| Error::Other(anyhow::anyhow!("xz footer compress: {e}")))?;
    enc.finish()
        .map_err(|e| Error::Other(anyhow::anyhow!("xz footer finish: {e}")))?;
    Ok(out)
}

/// Decompress a footer xz blob produced by [`compress_footer_bytes`].
pub fn decompress_footer_bytes(compressed: &[u8]) -> Result<Vec<u8>> {
    let mut dec = XzDecoder::new(compressed);
    let mut out = Vec::new();
    dec.read_to_end(&mut out)
        .map_err(|e| Error::Other(anyhow::anyhow!("xz footer decompress: {e}")))?;
    Ok(out)
}

pub struct InterruptibleXzEncoder<W: Write> {
    stream: Stream,
    obj: Option<W>,
    buf: Vec<u8>,
    shutdown: Shutdown,
}

impl<W: Write> InterruptibleXzEncoder<W> {
    pub fn new(
        file: W,
        jobs: usize,
        memlimit: Option<u64>,
        preset: u32,
        shutdown: Shutdown,
    ) -> Result<(Self, u32)> {
        let threads = resolve_xz_threads(jobs, memlimit, preset)?;

        let mut builder = MtStreamBuilder::new();
        builder
            .threads(threads)
            .preset(preset)
            .timeout_ms(TIMEOUT_MS)
            .block_size(BLOCK_SIZE)
            .check(Check::Crc64);

        let stream = builder.encoder().map_err(|e| {
            Error::Other(anyhow::anyhow!(
                "xz multithreaded encoder ({threads} threads): {e}"
            ))
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
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "xz output stalled",
                ));
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

// ---------------------------------------------------------------------------------------------------------------------
// TEST — footer xz round-trip (feature `test-compression`)
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(all(test, feature = "test-compression"))]
mod tests {
    use super::*;

    #[test]
    fn footer_bytes_roundtrip() {
        let blobs: [(&str, Vec<u8>); 3] = [
            ("empty", Vec::new()),
            ("small", crate::test_support::pseudo_random(crate::test_support::TEST_PAYLOAD_SIZE_BYTES)),
            ("large", crate::test_support::pseudo_random(1024 * 1024)),
        ];
        for (label, input) in blobs {
            let enc = compress_footer_bytes(&input).expect("compress footer");
            let dec = decompress_footer_bytes(&enc).expect("decompress footer");
            assert_eq!(dec, input, "footer xz round-trip mismatch ({label})");
        }
    }

    // #####################################################################
    // format_mib
    // #####################################################################

    #[test]
    fn format_mib_rounds_up_to_next_mib() {
        assert_eq!(format_mib(0), "0 MiB");
        assert_eq!(format_mib(1), "1 MiB");
        assert_eq!(format_mib(1024 * 1024), "1 MiB");
        // one byte past a full MiB rounds up.
        assert_eq!(format_mib(1024 * 1024 + 1), "2 MiB");
        // 1023 KiB rounds down to 1 MiB (ceil), not up to 2.
        assert_eq!(format_mib(1024 * 1024 - 1024), "1 MiB");
    }

    #[test]
    fn format_mib_groups_thousands_with_apostrophe() {
        assert_eq!(format_mib(1_048_576u64 * 1024 * 1024), "1'048'576 MiB");
        assert_eq!(format_mib(123_456_789u64 * 1024 * 1024), "123'456'789 MiB");
        assert_eq!(format_mib(999 * 1024 * 1024), "999 MiB");
    }

    // #####################################################################
    // mt_memusage
    // #####################################################################

    #[test]
    fn mt_memusage_is_non_decreasing_in_threads_and_preset() {
        let mut ub = 0u64;
        for threads in [1u32, 2, 4, 8, 16] {
            let u = mt_memusage(threads, 6);
            assert!(
                u >= ub,
                "memusage({threads}, 6) = {u} dropped below previous {ub}"
            );
            ub = u;
        }
        let low = mt_memusage(2, 1);
        let high = mt_memusage(2, 9);
        assert!(
            high >= low,
            "memusage(2, 9) = {high} below memusage(2, 1) = {low}"
        );
        let extreme = mt_memusage(2, 9 | PRESET_EXTREME);
        assert!(extreme >= high);
    }

    // #####################################################################
    // resolve_xz_threads
    // #####################################################################

    #[test]
    fn resolve_xz_threads_no_limit_passes_requested_through() {
        let preset = 6;
        for req in [1usize, 2, 4, 16] {
            assert_eq!(
                resolve_xz_threads(req, None, preset).unwrap(),
                req as u32,
                "requested {req}"
            );
        }
        // 0 clamps to 1 (requested.max(1)).
        assert_eq!(resolve_xz_threads(0, None, preset).unwrap(), 1);
    }

    #[test]
    fn resolve_xz_threads_roomy_memlimit_passes_through() {
        let preset = 6;
        let req = 4usize;
        let usage = mt_memusage(req as u32, preset);
        let threads = resolve_xz_threads(req, Some(usage + usage / 4), preset).unwrap();
        assert_eq!(threads, req as u32);
    }

    #[test]
    fn resolve_xz_threads_tight_memlimit_reduces_threads() {
        let preset = 6;
        let req = 8usize;
        let one = mt_memusage(1, preset);
        let two = mt_memusage(2, preset);
        // A limit between the 1-thread and 2-thread usage must squeeze to 1.
        let limit = one + (two - one) / 2;
        let threads = resolve_xz_threads(req, Some(limit), preset).unwrap();
        assert_eq!(threads, 1, "limit {limit} between one {one} and two {two}");
    }

    #[test]
    fn resolve_xz_threads_reduction_is_never_below_one() {
        for req in [2usize, 4, 32] {
            let preset = 9 | PRESET_EXTREME;
            let one = mt_memusage(1, preset);
            let threads = resolve_xz_threads(req, Some(one), preset).unwrap();
            assert_eq!(threads, 1, "req {req}");
        }
    }

    #[test]
    fn resolve_xz_threads_memlimit_below_single_thread_usage_errors() {
        let preset = 9 | PRESET_EXTREME;
        let one = mt_memusage(1, preset);
        let err = resolve_xz_threads(4, Some(one - 1), preset).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("raise --memlimit-compress"),
            "error should surface the hint: {msg}"
        );
        assert!(msg.contains("-9e"), "preset named with -e suffix: {msg}");
    }

    #[test]
    fn resolve_xz_threads_extreme_flag_reflected_in_error() {
        // Extreme bit shows as `-Ne` in the mandatory hint.
        let preset = 0 | PRESET_EXTREME;
        let err = resolve_xz_threads(1, Some(0), preset).unwrap_err();
        assert!(err.to_string().contains("-0e needs at least"));
    }

    // #####################################################################
    // InterruptibleXzEncoder
    // #####################################################################

    #[test]
    fn encoder_write_finish_roundtrips() {
        let shutdown = Shutdown::detached();
        let (enc, threads) = InterruptibleXzEncoder::new(
            Vec::new(), 1, None, 6, shutdown).expect("open encoder");
        assert_eq!(threads, 1);

        let mut enc = enc;
        let payload = crate::test_support::pseudo_random(64 * 1024);
        enc.write_all(&payload).expect("write all");
        let out = enc.finish().expect("finish");

        let mut dec = XzDecoder::new(&out[..]);
        let mut got = Vec::new();
        dec.read_to_end(&mut got).expect("decompress");
        assert_eq!(got, payload);
    }

    #[test]
    fn encoder_empty_input_roundtrips() {
        let shutdown = Shutdown::detached();
        let (enc, _) = InterruptibleXzEncoder::new(Vec::new(), 1, None, 6, shutdown)
            .expect("open encoder");
        let out = enc.finish().expect("finish empty");
        let mut dec = XzDecoder::new(&out[..]);
        let mut got = Vec::new();
        dec.read_to_end(&mut got).expect("decompress empty");
        assert!(got.is_empty());
    }

    #[test]
    fn encoder_flush_forwards_to_writer() {
        use std::rc::Rc;
        struct FlushProbe {
            flushed: Rc<std::cell::Cell<bool>>,
        }
        impl std::io::Write for FlushProbe {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.flushed.set(true);
                Ok(())
            }
        }
        let flushed = Rc::new(std::cell::Cell::new(false));
        let shutdown = Shutdown::detached();
        let (mut enc, _) = InterruptibleXzEncoder::new(
            FlushProbe { flushed: flushed.clone() }, 1, None, 6, shutdown)
            .expect("open encoder");
        enc.flush().expect("flush");
        assert!(flushed.get(), "flush must reach the underlying writer");
        drop(enc);
    }

    #[test]
    fn encoder_write_aborts_on_force() {
        let shutdown = Shutdown::detached();
        let (mut enc, _) = InterruptibleXzEncoder::new(Vec::new(), 1, None, 6, shutdown.clone())
            .expect("open encoder");
        shutdown.request_force();
        // NB: `write_all` retries on ErrorKind::Interrupted; observe a single
        // `write` call to surface the abort.
        let err = enc.write(b"some bytes to flush through the xz pipeline");
        let io = err.expect_err("write must abort");
        assert_eq!(io.kind(), std::io::ErrorKind::Interrupted);
    }

    #[test]
    fn encoder_try_finish_aborts_on_force() {
        let shutdown = Shutdown::detached();
        let (mut enc, _) = InterruptibleXzEncoder::new(Vec::new(), 1, None, 6, shutdown.clone())
            .expect("open encoder");
        enc.write_all(b"payload").expect("write");
        shutdown.request_force();
        let io = enc.try_finish().expect_err("finish must abort");
        assert_eq!(io.kind(), std::io::ErrorKind::Interrupted);
    }

    #[test]
    fn encoder_abandon_skips_stream_footer() {
        let shutdown = Shutdown::detached();
        let (mut enc, _) = InterruptibleXzEncoder::new(Vec::new(), 1, None, 6, shutdown)
            .expect("open encoder");
        let payload = crate::test_support::pseudo_random(64 * 1024);
        enc.write_all(&payload).expect("write");
        let out = enc.abandon();
        // No footer: the xz stream never reaches StreamEnd.
        let mut dec = XzDecoder::new(&out[..]);
        let mut got = Vec::new();
        assert!(dec.read_to_end(&mut got).is_err(), "abandoned stream decodes");
    }

    #[test]
    fn encoder_stalling_writer_yields_writezero() {
        struct Stalling;
        impl std::io::Write for Stalling {
            fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
                Ok(0)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let shutdown = Shutdown::detached();
        let (mut enc, _) = InterruptibleXzEncoder::new(Stalling, 1, None, 6, shutdown)
            .expect("open encoder");
        // Incompressible input large enough to force the MT encoder to emit output,
        // then complete the stream so every compressed byte reaches the stall.
        let payload = crate::test_support::pseudo_random(64 * 1024);
        enc.write_all(&payload).expect("write all");
        let err = enc.try_finish().expect_err("stall must be detected");
        assert_eq!(err.kind(), std::io::ErrorKind::WriteZero);
    }
}
