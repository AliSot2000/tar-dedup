use chrono::{DateTime, Utc};

use std::io;
use std::path::Path;

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
