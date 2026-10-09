//! Policy-aware UTF-8 conversion for OS paths.
//!
//! Default (relax = false) is strict: a path that cannot be represented as
//! UTF-8 aborts the run with a config error. With `--relax-utf8` such an entry
//! is skipped instead (`Ok(None)`), and the caller reports the lossy name.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// A type that can fail UTF-8 conversion according to the relax policy.
///
/// - `Ok(Some(s))`: path converted to UTF-8.
/// - `Ok(None)`: not UTF-8, but relaxing is allowed — the caller skips the entry.
/// - `Err`: not UTF-8 and we are in strict mode — the run aborts.
pub trait PathText {
    fn to_utf8(&self, relax: bool) -> Result<Option<String>>;
}

/// Policy-aware UTF-8 check for a path that may be skipped instead of fatal.
///
/// Mirror of `PathText::to_utf8`: `None` = fine, `Some(Ok(()))` = non-UTF-8
/// skipped under `relax` (already warned), `Some(Err)` = non-UTF-8 fatal.
pub fn sanitize_path(relax: bool, path: impl AsRef<Path>, path_type: &str)
    -> Option<Result<()>> {
    let path = path.as_ref();
    match path.to_utf8(relax) {
        Ok(Some(_)) => None,
        Ok(None) => {
            tracing::warn!("Skipping non-UTF-8 {path_type}-path: {}", path.to_string_lossy());
            Some(Ok(()))
        }
        Err(e) => Some(Err(e)),
    }
}

/// Strict variant for db-store call sites. Paths handed to the store have
/// already passed an entry boundary, so a failure here is an integrity abort,
/// not a skip.
pub fn require_utf8(path: impl AsRef<Path>) -> Result<String> {
    match utf8_of_os(path.as_ref().as_os_str()) {
        Some(s) => Ok(s),
        None => Err(non_utf8_error()),
    }
}

fn non_utf8_error() -> Error {
    Error::Config("Only UTF-8 Strings supported. Disable with --relax-utf8".into())
}

fn utf8_of_os(s: &OsStr) -> Option<String> {
    s.to_str().map(|s| s.to_string())
}

fn utf8_policy(s: &OsStr, relax: bool) -> Result<Option<String>> {
    match utf8_of_os(s) {
        Some(s) => Ok(Some(s)),
        None => if relax { Ok(None) } else { Err(non_utf8_error()) },
    }
}

impl PathText for Path {
    fn to_utf8(&self, relax: bool) -> Result<Option<String>> {
        utf8_policy(self.as_os_str(), relax)
    }
}

impl PathText for PathBuf {
    fn to_utf8(&self, relax: bool) -> Result<Option<String>> {
        utf8_policy(self.as_os_str(), relax)
    }
}

impl PathText for OsStr {
    fn to_utf8(&self, relax: bool) -> Result<Option<String>> {
        utf8_policy(self, relax)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn non_utf8_path() -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        // `b` followed by two invalid continuation bytes.
        let os = OsStr::from_bytes(&[0x62, 0xFF, 0xFE]);
        PathBuf::from(&std::borrow::Cow::Borrowed(os))
    }

    #[test]
    fn valid_path_converts() {
        let path = PathBuf::from("/tmp/ok");
        assert_eq!(path.to_utf8(false).unwrap(), Some("/tmp/ok".to_string()));
        assert_eq!(path.to_utf8(true).unwrap(), Some("/tmp/ok".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn strict_invalid_errors() {
        let path = non_utf8_path();
        let err = path.to_utf8(false).unwrap_err();
        assert!(err.to_string().contains("--relax-utf8"));
    }

    #[cfg(unix)]
    #[test]
    fn relax_invalid_skips() {
        let path = non_utf8_path();
        assert_eq!(path.to_utf8(true).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn require_utf8_errors_on_invalid() {
        let path = non_utf8_path();
        assert!(require_utf8(&path).is_err());
    }

    #[test]
    fn require_utf8_ok_on_valid() {
        let path = PathBuf::from("/tmp/ok");
        assert_eq!(require_utf8(&path).unwrap(), "/tmp/ok");
    }
}