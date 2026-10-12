//! Policy-aware UTF-8 conversion for OS paths entering the database.
//!
//! The canonical working form is a UTF-8 string (`SanitizedPath::text`), which is
//! the lossy rendering under `--relax-utf8`; the original on-disk bytes are always
//! kept alongside (`SanitizedPath::raw`) for archive-time filesystem access and for
//! a faithful database record. Raw bytes are never used as working paths.
//!
//! Policy matrix:
//! - valid UTF-8 (any mode)           -> proceed with `text` + `raw`
//! - non-UTF-8 + strict               -> warn + skip (`Ok(None)`)
//! - non-UTF-8 + strict + fail-fast   -> abort (`Err`)
//! - non-UTF-8 + relax (any fail-fast) -> lossy `text`, never skip

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Outcome of applying the UTF-8 policy to a path entering the database.
#[derive(Debug, Clone)]
pub struct SanitizedPath {
    /// Canonical working string (valid UTF-8; the lossy rendering under relax).
    pub text: String,
    /// Original on-disk bytes, kept for fs access and inspection.
    pub raw: PathBuf,
}

/// Apply the UTF-8 policy to a path entering the database.
///
/// Returns `Ok(Some(SanitizedPath))` to proceed (use `.text` for the DB, `.raw`
/// for filesystem access), `Ok(None)` to skip (strict, no fail-fast), or `Err`
/// to abort (strict with fail-fast).
pub fn sanitize_path(relax: bool, fail_fast: bool, path: impl AsRef<Path>, path_type: &str)
    -> Result<Option<SanitizedPath>> {
    let path = path.as_ref();
    match path.as_os_str().to_str() {
        Some(text) => Ok(Some(SanitizedPath {
            text: text.to_string(),
            raw: path.to_path_buf(),
        })),
        None if relax => {
            tracing::warn!(
                "Lossy-converting non-UTF-8 {path_type}-path: {}",
                path.to_string_lossy()
            );
            Ok(Some(SanitizedPath {
                text: path.to_string_lossy().into_owned(),
                raw: path.to_path_buf(),
            }))
        }
        None if fail_fast => Err(Error::Config(format!(
            "Non-UTF-8 {path_type}-path aborted by --fail-fast: {}",
            path.to_string_lossy()
        ))),
        None => {
            tracing::warn!(
                "Skipping non-UTF-8 {path_type}-path: {}",
                path.to_string_lossy()
            );
            Ok(None)
        }
    }
}

/// Strict UTF-8 conversion for assert sites (paths already validated upstream,
/// or content known to be valid UTF-8 by construction). A failure here is an
/// integrity abort, not a skip.
pub fn require_utf8(path: impl AsRef<Path>) -> Result<String> {
    match path.as_ref().as_os_str().to_str() {
        Some(s) => Ok(s.to_string()),
        None => Err(Error::Config("Only UTF-8 Strings supported.".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn non_utf8_path() -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        // `b` followed by two invalid continuation bytes.
        let os = std::ffi::OsStr::from_bytes(&[0x62, 0xFF, 0xFE]);
        PathBuf::from(&std::borrow::Cow::Borrowed(os))
    }

    fn text_of(res: Option<&SanitizedPath>) -> Option<&str> {
        res.map(|s| s.text.as_str())
    }

    #[test]
    fn valid_path_proceeds_in_all_modes() {
        let path = PathBuf::from("/tmp/ok");
        assert_eq!(text_of(sanitize_path(false, false, &path, "test").unwrap().as_ref()),
                   Some("/tmp/ok"));
        assert_eq!(text_of(sanitize_path(false, true, &path, "test").unwrap().as_ref()),
                   Some("/tmp/ok"));
        assert_eq!(text_of(sanitize_path(true, false, &path, "test").unwrap().as_ref()),
                   Some("/tmp/ok"));
        assert_eq!(text_of(sanitize_path(true, true, &path, "test").unwrap().as_ref()),
                   Some("/tmp/ok"));
    }

    #[cfg(unix)]
    #[test]
    fn strict_skips_invalid() {
        let path = non_utf8_path();
        assert!(sanitize_path(false, false, &path, "test").unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn strict_fail_fast_aborts_invalid() {
        let path = non_utf8_path();
        let err = sanitize_path(false, true, &path, "test").unwrap_err();
        assert!(err.to_string().contains("--fail-fast"));
    }

    #[cfg(unix)]
    #[test]
    fn relax_lossy_converts_invalid() {
        let path = non_utf8_path();
        let san = sanitize_path(true, false, &path, "test").unwrap().unwrap();
        assert_eq!(san.text, "b\u{FFFD}\u{FFFD}");
        assert_eq!(san.raw, path);
    }

    #[cfg(unix)]
    #[test]
    fn relax_fail_fast_lossy_converts_invalid() {
        let path = non_utf8_path();
        let san = sanitize_path(true, true, &path, "test").unwrap().unwrap();
        assert_eq!(san.text, "b\u{FFFD}\u{FFFD}");
        assert_eq!(san.raw, path);
    }

    #[cfg(unix)]
    #[test]
    fn require_utf8_errors_on_invalid() {
        assert!(require_utf8(&non_utf8_path()).is_err());
    }

    #[test]
    fn require_utf8_ok_on_valid() {
        assert_eq!(require_utf8(PathBuf::from("/tmp/ok")).unwrap(), "/tmp/ok");
    }
}
