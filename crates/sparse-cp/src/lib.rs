//! Sparse-aware copy and zero-block scanning.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use filetime::{set_file_times, FileTime};

/// Result of a sparse copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SparseCopyStats {
    /// Logical size of the source (and destination after copy).
    pub size_in: u64,
    /// Allocated size of the destination after copy (best-effort).
    pub size_out: u64,
    /// Bytes skipped because whole blocks were zeros (`zero_blocks * block_size`, plus
    /// any short all-zero tail that was seeked over).
    pub bytes_saved: u64,
    /// Number of full `block_size` reads that were entirely zero.
    pub zero_blocks: u64,
}

/// Count how many full `block_size` chunks of `path` are entirely `0x00`.
///
/// A short final chunk that is all zeros is **not** counted (it is not a full block).
pub fn sparse_page_count(path: &Path, block_size: usize) -> io::Result<u64> {
    if block_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "block_size must be > 0",
        ));
    }

    let mut file = File::open(path)?;
    let mut buf = vec![0u8; block_size];
    let mut count = 0u64;

    loop {
        let n = read_fullish(&mut file, &mut buf)?;
        if n == 0 {
            break;
        }
        if n == block_size && is_all_zero(&buf[..n]) {
            count += 1;
        }
        if n < block_size {
            break;
        }
    }

    Ok(count)
}

/// Like [`sparse_page_count`], but reports progress via `on_progress`.
///
/// `on_progress` may return `Err` to abort early. IO failures are converted with [`From::from`].
pub fn sparse_page_count_with_progress<E, F>(
    path: &Path,
    block_size: usize,
    mut on_progress: F,
) -> Result<u64, E>
where
    F: FnMut(u64, u64, Duration) -> Result<(), E>,
    E: From<io::Error>,
{
    if block_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "block_size must be > 0",
        )
        .into());
    }

    let size_in = fs::metadata(path)?.len();
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; block_size];
    let mut count = 0u64;
    let mut pos = 0u64;
    let start = Instant::now();
    on_progress(0, size_in, Duration::ZERO)?;

    loop {
        let n = read_fullish(&mut file, &mut buf)?;
        if n == 0 {
            break;
        }
        if n == block_size && is_all_zero(&buf[..n]) {
            count += 1;
        }
        pos += n as u64;
        on_progress(pos, size_in, start.elapsed())?;
        if n < block_size {
            break;
        }
    }

    Ok(count)
}

/// Copy `src` → `dst` sparsely: metadata, truncate to size, write only non-zero blocks.
pub fn sparse_copy(src: &Path, dst: &Path, block_size: usize) -> io::Result<SparseCopyStats> {
    sparse_copy_with_progress(src, dst, block_size, |_, _, _| Ok::<(), io::Error>(()))
}

/// Same as [`sparse_copy`], with a progress callback after each block.
///
/// `on_progress` may return `Err` to abort early (partial `dst` may exist). IO failures are
/// converted with [`From::from`].
pub fn sparse_copy_with_progress<E, F>(
    src: &Path,
    dst: &Path,
    block_size: usize,
    mut on_progress: F,
) -> Result<SparseCopyStats, E>
where
    F: FnMut(u64, u64, Duration) -> Result<(), E>,
    E: From<io::Error>,
{
    if block_size == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "block_size must be > 0",
        )
        .into());
    }

    let size_in = fs::metadata(src)?.len();
    copy_metadata_only(src, dst)?;

    let mut out = OpenOptions::new().write(true).open(dst)?;
    out.set_len(size_in)?;

    let mut inp = File::open(src)?;
    let mut buf = vec![0u8; block_size];
    let mut pos = 0u64;
    let mut zero_blocks = 0u64;
    let mut bytes_saved = 0u64;
    let start = Instant::now();
    on_progress(0, size_in, Duration::ZERO)?;

    loop {
        let n = read_fullish(&mut inp, &mut buf)?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        if is_all_zero(chunk) {
            if n == block_size {
                zero_blocks += 1;
            }
            bytes_saved += n as u64;
            pos += n as u64;
            out.seek(SeekFrom::Start(pos))?;
        } else {
            out.write_all(chunk)?;
            pos += n as u64;
        }
        on_progress(pos, size_in, start.elapsed())?;
    }

    // Ensure logical size is exact even if last ops were seeks.
    out.set_len(size_in)?;
    out.sync_all()?;
    drop(out);

    let size_out = allocated_size(dst).unwrap_or(size_in);

    Ok(SparseCopyStats {
        size_in,
        size_out,
        bytes_saved,
        zero_blocks,
    })
}

/// Create `dst` (or truncate) and copy mode / timestamps from `src` (no file data).
pub fn copy_metadata_only(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::metadata(src)?;
    {
        let _f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(dst)?;
    }

    let perms = meta.permissions();
    fs::set_permissions(dst, perms)?;

    let atime = FileTime::from_system_time(meta.accessed()?);
    let mtime = FileTime::from_system_time(meta.modified()?);
    set_file_times(dst, atime, mtime)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::{chown, MetadataExt};
        let _ = chown(dst, Some(meta.uid()), Some(meta.gid()));
    }

    Ok(())
}

/// Best-effort allocated size (Unix: `st_blocks * 512`; elsewhere: logical length).
pub fn allocated_size(path: &Path) -> io::Result<u64> {
    let meta = fs::metadata(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return Ok(meta.blocks() * 512);
    }
    #[cfg(not(unix))]
    {
        Ok(meta.len())
    }
}

fn is_all_zero(chunk: &[u8]) -> bool {
    chunk.iter().all(|&b| b == 0)
}

/// Read until `buf` is full or EOF; returns bytes read (may be short only at EOF).
fn read_fullish(file: &mut File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::io::{Seek, SeekFrom, Write};
    use tempfile::NamedTempFile;

    const GIB: u64 = 1024 * 1024 * 1024;
    const PAGE_4K: usize = 4096;
    /// 1 GiB / 4096
    const PAGES_1G: usize = 262_144;
    const POPULATED: usize = 2_144;
    /// Fully empty 4KiB pages in a 1 GiB file after populating `POPULATED` pages.
    /// Note: 262_144 − 2_144 = **260_000** (not 26_000).
    const EMPTY_4K_PAGES: u64 = (PAGES_1G - POPULATED) as u64;

    #[test]
    fn counts_full_zero_blocks_only() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(&[0u8; 8192 + 100]).unwrap();
        f.flush().unwrap();

        assert_eq!(sparse_page_count(f.path(), 4096).unwrap(), 2);
    }

    #[test]
    fn sparse_copy_skips_zero_blocks() {
        let mut src = NamedTempFile::new().unwrap();
        let mut data = vec![1u8; 4096];
        data.extend_from_slice(&[0u8; 8192]);
        data.extend_from_slice(&[2u8; 4096]);
        src.write_all(&data).unwrap();
        src.flush().unwrap();

        let dst = NamedTempFile::new().unwrap();
        let dst_path = dst.path().to_path_buf();
        drop(dst);

        let stats = sparse_copy(src.path(), &dst_path, 4096).unwrap();
        assert_eq!(stats.size_in, data.len() as u64);
        assert_eq!(stats.zero_blocks, 2);
        assert_eq!(stats.bytes_saved, 8192);

        let copied = fs::read(&dst_path).unwrap();
        assert_eq!(copied, data);
    }

    /// 512 B of non-zero data → no full zero pages at the default 4KiB block size.
    #[test]
    fn list_only_nonzero_512_reports_zero_pages() {
        let mut f = NamedTempFile::new().unwrap();
        let mut seed = 0xC0FFEE_u64;
        let mut buf = [0u8; 512];
        fill_nonzero(&mut buf, &mut seed);
        assert!(buf.iter().all(|&b| b != 0));
        f.write_all(&buf).unwrap();
        f.flush().unwrap();

        assert_eq!(sparse_page_count(f.path(), PAGE_4K).unwrap(), 0);
    }

    /// Pattern `(4095 × 0x00) ++ 0x01` repeated 4×.
    ///
    /// - 4096: each aligned page ends in 0x01 → 0 empty pages  
    /// - 2048: first half empty, second half not → 4 empty pages  
    /// - 1024: three empty quarters, last not → 12 empty pages  
    #[test]
    fn list_only_almost_full_zero_pages_by_blocksize() {
        let mut f = NamedTempFile::new().unwrap();
        let mut pattern = vec![0u8; PAGE_4K];
        pattern[PAGE_4K - 1] = 0x01;
        for _ in 0..4 {
            f.write_all(&pattern).unwrap();
        }
        f.flush().unwrap();

        assert_eq!(sparse_page_count(f.path(), 4096).unwrap(), 0);
        assert_eq!(sparse_page_count(f.path(), 2048).unwrap(), 4);
        assert_eq!(sparse_page_count(f.path(), 1024).unwrap(), 12);
    }

    /// Dense 1 GiB file; 2144 random 4KiB pages filled with non-zero bytes.
    /// Empty 4KiB pages: 262_144 − 2_144 = 260_000.
    #[test]
    fn list_only_1gib_fully_populated_pages() {
        let path = dense_zero_file_1gib();
        let _cleanup = RemovePath(path.clone());
        let mut seed = 0xA11CE_u64;
        let chosen = pick_unique_indices(POPULATED, PAGES_1G, &mut seed);

        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        let mut page = [0u8; PAGE_4K];
        for &idx in &chosen {
            fill_nonzero(&mut page, &mut seed);
            file.seek(SeekFrom::Start((idx * PAGE_4K) as u64)).unwrap();
            file.write_all(&page).unwrap();
        }
        file.flush().unwrap();
        drop(file);

        assert_eq!(
            sparse_page_count(&path, PAGE_4K).unwrap(),
            EMPTY_4K_PAGES
        );
    }

    /// Dense 1 GiB; 2144 pages each get 1024 B non-zero at a random 1KiB-aligned offset.
    ///
    /// - 4096 → 260_000 empty (untouched full pages only)  
    /// - 2048 → 520_000 + 2_144 = 522_144 (each dirty page still has one empty half)  
    /// - 1024 → 1_040_000 + 2_144×3 = 1_046_432  
    #[test]
    fn list_only_1gib_partial_1k_writes() {
        let path = dense_zero_file_1gib();
        let _cleanup = RemovePath(path.clone());
        let mut seed = 0xBEEF_u64;
        let chosen = pick_unique_indices(POPULATED, PAGES_1G, &mut seed);

        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        let mut chunk = [0u8; 1024];
        for &idx in &chosen {
            let slot = (next_u64(&mut seed) % 4) as u64;
            let offset = (idx as u64) * PAGE_4K as u64 + slot * 1024;
            fill_nonzero(&mut chunk, &mut seed);
            file.seek(SeekFrom::Start(offset)).unwrap();
            file.write_all(&chunk).unwrap();
        }
        file.flush().unwrap();
        drop(file);

        assert_eq!(sparse_page_count(&path, 4096).unwrap(), EMPTY_4K_PAGES);
        assert_eq!(sparse_page_count(&path, 2048).unwrap(), 522_144);
        assert_eq!(sparse_page_count(&path, 1024).unwrap(), 1_046_432);
    }

    struct RemovePath(std::path::PathBuf);
    impl Drop for RemovePath {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    /// Allocate a dense 1 GiB file of zeros (actually written, not a hole).
    fn dense_zero_file_1gib() -> std::path::PathBuf {
        let mut f = NamedTempFile::new().unwrap();
        let zeros = vec![0u8; 1024 * 1024]; // 1 MiB
        let mut written = 0u64;
        while written < GIB {
            let n = ((GIB - written) as usize).min(zeros.len());
            f.write_all(&zeros[..n]).unwrap();
            written += n as u64;
        }
        f.flush().unwrap();
        f.into_temp_path().keep().expect("persist temp 1GiB file")
    }

    fn fill_nonzero(buf: &mut [u8], seed: &mut u64) {
        for b in buf {
            // Never emit 0x00.
            *b = (next_u64(seed) % 255) as u8 + 1;
        }
    }

    fn next_u64(seed: &mut u64) -> u64 {
        // SplitMix64-ish
        *seed = seed.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = *seed;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    fn pick_unique_indices(count: usize, range: usize, seed: &mut u64) -> Vec<usize> {
        assert!(count <= range);
        // Partial Fisher–Yates over 0..range
        let mut pool: Vec<usize> = (0..range).collect();
        for i in 0..count {
            let j = i + (next_u64(seed) as usize % (range - i));
            pool.swap(i, j);
        }
        let mut out = pool[..count].to_vec();
        out.sort_unstable();
        // uniqueness sanity
        let set: HashSet<_> = out.iter().copied().collect();
        assert_eq!(set.len(), count);
        out
    }
}

