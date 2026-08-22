use chrono::{DateTime, Utc};

use std::io;
use std::path::Path;

/// True when either cleaned absolute path is a component-wise prefix of the other.
///
/// `/a` overlaps `/a/b` (both orders) and `/a` overlaps `/a`. `/a/b` vs `/a/c`
/// and `/a` vs `/ab` do not.
pub fn directory_roots_overlap(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
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
        assert!(directory_roots_overlap(Path::new("/a"), Path::new("/a/b")));
        assert!(directory_roots_overlap(Path::new("/a/b"), Path::new("/a")));
    }

    #[test]
    fn overlap_identical() {
        assert!(directory_roots_overlap(Path::new("/a"), Path::new("/a")));
    }

    #[test]
    fn siblings_do_not_overlap() {
        assert!(!directory_roots_overlap(Path::new("/a/b"), Path::new("/a/c")));
    }

    #[test]
    fn string_prefix_is_not_overlap() {
        assert!(!directory_roots_overlap(Path::new("/a"), Path::new("/ab")));
    }

    #[test]
    fn trailing_slash_clean_equivalent() {
        assert!(directory_roots_overlap(Path::new("/a/b"), Path::new("/a/b/")));
        assert!(directory_roots_overlap(Path::new("/a/b/"), Path::new("/a/b")));
    }
}