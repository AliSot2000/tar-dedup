//! Shared streaming hashing: a `Read` wrapper that feeds every byte pulled
//! through it into a sha1 digest, transparently. The caller keeps its own read
//! loop (zero-page scan, shutdown checks, byte limit, progress bar) and the
//! bytes are still visible to it; the digest is extracted at the end with
//! [`HashingReader::finalize`].

use sha1::{Digest, Sha1};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::common::defaults::ARCHIVE_IO_BUF_SIZE;
use crate::error::{Error, Result};
use crate::progress::ProgressBar;
use crate::shutdown::Shutdown;

/// Transparent `Read` wrapper: reads through `inner`, hashing every byte.
/// Owns no buffer and imposes no loop shape; `finalize(self)` returns the digest.
pub struct HashingReader<R> {
    inner: R,
    hasher: Sha1,
}

impl<R> HashingReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha1::new(),
        }
    }

    /// Consume the reader and return the sha1 of everything read so far.
    pub fn finalize(self) -> [u8; 20] {
        self.hasher.finalize().into()
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        Ok(n)
    }
}

/// Stream-hash the `[start, start+len)` range of `path` with sha1, chunked at
/// `ARCHIVE_IO_BUF_SIZE`. Checks `shutdown` per chunk (a force abort surfaces
/// `Error::Interrupted`), reporting progress on `bar` (which the caller may have
/// pre-sized). The single home for range-hashing: footer write, extract verify,
/// and `check` all delegate here.
pub fn hash_file_range(
    path: &Path,
    start: u64,
    len: u64,
    shutdown: &Shutdown,
    bar: Option<&ProgressBar>)
    -> Result<[u8; 20]> {

    let mut file = File::open(path).map_err(|e| Error::io(path, e))?;
    file.seek(SeekFrom::Start(start))
        .map_err(|e| Error::io(path, e))?;
    let mut reader = HashingReader::new(file);
    let mut buf = vec![0u8; ARCHIVE_IO_BUF_SIZE];
    let mut remaining = len;
    while remaining > 0 {
        shutdown.check_in_flight()?;
        let want = remaining.min(buf.len() as u64) as usize;
        let n = reader
            .read(&mut buf[..want])
            .map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        remaining -= n as u64;
        if let Some(bar) = bar {
            bar.inc(n as u64);
        }
    }
    Ok(reader.finalize())
}

//==================================================================================================
// TEST
//==================================================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use sha1::{Digest, Sha1};

    fn sha1_of(payload: &[u8]) -> [u8; 20] {
        let mut hasher = Sha1::new();
        hasher.update(payload);
        hasher.finalize().into()
    }

    #[test]
    fn hash_file_range_matches_whole_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("a.bin");
        let payload: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &payload).expect("write");

        let got = hash_file_range(&path, 0, payload.len() as u64, &Shutdown::detached(), None)
            .expect("hash");
        assert_eq!(got, sha1_of(&payload));
    }

    #[test]
    fn hash_file_range_respects_start_and_len() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("b.bin");
        let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&path, &payload).expect("write");

        let start = 3_000usize;
        let len = 12_345usize;
        let got = hash_file_range(&path, start as u64, len as u64, &Shutdown::detached(), None)
            .expect("hash");
        assert_eq!(got, sha1_of(&payload[start..start + len]));
    }

    #[test]
    fn hash_file_range_force_interrupt_aborts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("c.bin");
        // Large enough to span multiple chunks, so a force lands mid-loop.
        std::fs::write(&path, vec![0u8; 8 * ARCHIVE_IO_BUF_SIZE]).expect("write");

        let sh = Shutdown::detached();
        sh.request_force();
        let err = hash_file_range(&path, 0, 8 * ARCHIVE_IO_BUF_SIZE as u64, &sh, None)
            .expect_err("force must abort");
        assert!(matches!(err, Error::Interrupted), "{err:?}");
    }

    #[test]
    fn hash_file_range_reports_bar_progress() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("d.bin");
        let payload = vec![7u8; 2 * ARCHIVE_IO_BUF_SIZE + 123];
        std::fs::write(&path, &payload).expect("write");

        let bar = ProgressBar::new(0);
        bar.set_length(payload.len() as u64);
        let got = hash_file_range(&path, 0, payload.len() as u64, &Shutdown::detached(), Some(&bar))
            .expect("hash");
        assert_eq!(got, sha1_of(&payload));
        assert_eq!(bar.position(), payload.len() as u64);
    }
}
