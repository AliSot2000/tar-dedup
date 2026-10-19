//! Shared streaming hashing: a `Read` wrapper that feeds every byte pulled
//! through it into a sha1 digest, transparently. The caller keeps its own read
//! loop (zero-page scan, shutdown checks, byte limit, progress bar) and the
//! bytes are still visible to it; the digest is extracted at the end with
//! [`HashingReader::finalize`].

use sha1::{Digest, Sha1};
use std::io::Read;

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
