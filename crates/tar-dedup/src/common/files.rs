use chrono::{DateTime, Utc};

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::Path;

use crate::error::{Error, Result};

use super::COPY_STEP_SIZE;

/// Copy `src` → `dst` in [`COPY_STEP_SIZE`] chunks, calling `should_interrupt` after each chunk.
///
/// When `should_interrupt()` returns `true`, the partial `dst` is removed and [`Error::Interrupted`]
/// is returned. When it returns `false`, copying continues until EOF.
pub fn copy_file_batched<P, F>(src: P, dst: P, mut should_interrupt: F) -> Result<()>
where
    P: AsRef<Path>,
    F: FnMut() -> bool,
{
    let src = src.as_ref();
    let dst = dst.as_ref();

    let mut src_file = File::open(src).map_err(|e| Error::io(src, e))?;
    let mut dst_file = File::create(dst).map_err(|e| Error::io(dst, e))?;

    let mut buf = vec![0u8; COPY_STEP_SIZE as usize];
    loop {
        let n = src_file.read(&mut buf).map_err(|e| Error::io(src, e))?;
        if n == 0 {
            break;
        }
        dst_file
            .write_all(&buf[..n])
            .map_err(|e| Error::io(dst, e))?;
        if should_interrupt() {
            let _ = fs::remove_file(dst);
            return Err(Error::Interrupted);
        }
    }
    Ok(())
}

/// True when two directory roots collide for inventory.
///
/// Same path (including trailing `/`) always overlaps. Nested paths (`/a` vs
/// `/a/b`) overlap only when `no_recursion` is false — `--no-recurse` walks
/// only the root itself, so a descendant root is a distinct tree.
/// `/a/b` vs `/a/c` and `/a` vs `/ab` never overlap.
pub fn directory_roots_overlap(a: &Path, b: &Path, no_recursion: bool) -> bool {
    if paths_equal_ignore_trailing_slash(a, b) {
        return true;
    }
    if no_recursion {
        return false;
    }
    a.starts_with(b) || b.starts_with(a)
}

fn paths_equal_ignore_trailing_slash(a: &Path, b: &Path) -> bool {
    let a = a.as_os_str().as_encoded_bytes();
    let b = b.as_os_str().as_encoded_bytes();
    a.strip_suffix(b"/").unwrap_or(a) == b.strip_suffix(b"/").unwrap_or(b)
}

/// Extension with leading dot (e.g. `.txt`), or empty if none / non-UTF-8.
pub fn original_extension(path: &Path) -> String {
    path.extension()
        .and_then(|ext| ext.to_str())
        .filter(|ext| !ext.is_empty())
        .map(|ext| format!(".{ext}"))
        .unwrap_or_default()
}

/// Iterator adapter: run `pre` on each item **immediately before** yielding it.
///
/// Paired with rayon `par_bridge()`, this means the check runs when a worker is
/// about to take the item — not in a bulk scan hours before that file is hashed.
pub struct PreYield<I, F> {
    inner: I,
    pre: F,
}

impl<I, F> PreYield<I, F> {
    pub fn new(inner: I, pre: F) -> Self {
        Self { inner, pre }
    }
}

impl<I, F> Iterator for PreYield<I, F>
where
    I: Iterator,
    F: FnMut(&I::Item),
{
    type Item = I::Item;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.inner.next()?;
        (self.pre)(&item);
        Some(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.inner.size_hint()
    }
}

impl<I, F> ExactSizeIterator for PreYield<I, F>
where
    I: ExactSizeIterator,
    F: FnMut(&I::Item),
{
}

/// Heuristic check: compare live timestamps to values captured at inventory.
///
/// Emits a warning if any recorded stamp differs — useful for catching accidental
/// in-tree edits (`sed`, `cat >>`, …). Does **not** fail the caller.
///
/// Note: our own reads often bump **atime**; a lone atime mismatch is usually
/// self-inflicted, but we still report it when recorded.
pub fn warn_if_times_changed(
    path: &Path,
    mtime: Option<DateTime<Utc>>,
    atime: Option<DateTime<Utc>>,
    ctime: Option<DateTime<Utc>>,
) {
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "could not stat file for timestamp check"
            );
            return;
        }
    };

    let (live_mtime, live_atime, live_ctime) = get_file_times(&meta);
    let mut changed = Vec::new();

    push_if_changed(&mut changed, "mtime", mtime, live_mtime);
    push_if_changed(&mut changed, "atime", atime, live_atime);
    push_if_changed(&mut changed, "ctime", ctime, live_ctime);

    if !changed.is_empty() {
        tracing::warn!(
            path = %path.display(),
            changed = %changed.join(","),
            "file timestamps changed since inventory (possible concurrent modification)"
        );
    }
}

/// Add the `name` to the `out` iff expected != live and both destruct correctly to Ok/Some.
fn push_if_changed(
    out: &mut Vec<&'static str>,
    name: &'static str,
    expected: Option<DateTime<Utc>>,
    live: io::Result<DateTime<Utc>>,
) {
    let (Some(expected), Ok(live)) = (expected, live) else {
        return;
    };
    // Second resolution: avoids false positives from FS vs RFC3339 subsecond noise.
    if expected.timestamp() != live.timestamp() {
        out.push(name);
    }
}

/// Get all times associated with the file. Result is `(mtime, atime, ctime)`.
pub fn get_file_times(
    meta: &std::fs::Metadata,
) -> (
    io::Result<DateTime<Utc>>,
    io::Result<DateTime<Utc>>,
    io::Result<DateTime<Utc>>,
) {
    (file_mtime(meta), file_atime(meta), file_ctime(meta))
}

fn file_mtime(meta: &std::fs::Metadata) -> io::Result<DateTime<Utc>> {
    Ok(DateTime::<Utc>::from(meta.modified()?))
}

fn file_atime(meta: &std::fs::Metadata) -> io::Result<DateTime<Utc>> {
    Ok(DateTime::<Utc>::from(meta.accessed()?))
}

fn file_ctime(meta: &std::fs::Metadata) -> io::Result<DateTime<Utc>> {
    Ok(DateTime::<Utc>::from(meta.created()?))
}

// INFO: Section copied from WalkDir::util, (what works - works)
#[cfg(unix)]
pub fn device_num_stat<P: AsRef<Path>>(path: P) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    path.as_ref().metadata().map(|md| md.dev())
}

#[cfg(unix)]
pub fn device_num_lstat<P: AsRef<Path>>(path: P) -> io::Result<u64> {
    use std::os::unix::fs::MetadataExt;
    path.as_ref().symlink_metadata().map(|md| md.dev())
}

/* TODO Handle Windows Version.
#[cfg(windows)]
pub fn device_num<P: AsRef<Path>>(path: P) -> io::Result<u64> {
    use winapi_util::{file, Handle};

    let h = Handle::from_path_any(path)?;
    file::information(h).map(|info| info.volume_serial_number())
}

#[cfg(not(any(unix, windows)))]
pub fn device_num<P: AsRef<Path>>(_: P) -> io::Result<u64> {
    Err(io::Error::new(
        io::ErrorKind::Other,
        "walkdir: same_file_system option not supported on this platform",
    ))
}
*/

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn overlap_nested_both_orders() {
        assert!(directory_roots_overlap(Path::new("/a"), Path::new("/a/b"), false));
        assert!(directory_roots_overlap(Path::new("/a/b"), Path::new("/a"), false));
    }

    #[test]
    fn overlap_identical() {
        assert!(directory_roots_overlap(Path::new("/a"), Path::new("/a"), false));
        assert!(directory_roots_overlap(Path::new("/a"), Path::new("/a"), true));
    }

    #[test]
    fn siblings_do_not_overlap() {
        assert!(!directory_roots_overlap(Path::new("/a/b"), Path::new("/a/c"), false));
        assert!(!directory_roots_overlap(Path::new("/a/b"), Path::new("/a/c"), true));
    }

    #[test]
    fn string_prefix_is_not_overlap() {
        assert!(!directory_roots_overlap(Path::new("/a"), Path::new("/ab"), false));
    }

    #[test]
    fn trailing_slash_clean_equivalent() {
        assert!(directory_roots_overlap(Path::new("/a/b"), Path::new("/a/b/"), false));
        assert!(directory_roots_overlap(Path::new("/a/b/"), Path::new("/a/b"), true));
    }

    #[test]
    fn no_recursion_ignores_descendants() {
        assert!(!directory_roots_overlap(Path::new("/a"), Path::new("/a/b"), true));
        assert!(!directory_roots_overlap(Path::new("/a/b"), Path::new("/a"), true));
    }

    #[test]
    fn copy_file_batched_copies_all_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        let payload: Vec<u8> = (0..COPY_STEP_SIZE as usize + 123).map(|i| (i % 251) as u8).collect();
        fs::write(&src, &payload).expect("write src");

        copy_file_batched(&src, &dst, || false).expect("copy");

        assert_eq!(fs::read(&dst).expect("read dst"), payload);
    }

    #[test]
    fn copy_file_batched_interrupt_removes_partial_dst() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        let payload = vec![0u8; COPY_STEP_SIZE as usize + 1];
        fs::write(&src, &payload).expect("write src");

        let mut chunks = 0u32;
        let err = copy_file_batched(&src, &dst, || {
            chunks += 1;
            chunks >= 2
        })
        .expect_err("interrupted");

        assert!(matches!(err, Error::Interrupted));
        assert!(!dst.exists());
    }
}