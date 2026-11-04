use crate::common::xattr::PosixQualifierParserError;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

/// Wrap a `Result`-returning free fn so `Err` is constrained to `$allowed`
/// (an or-pattern). Debug builds panic on disallowed variants; the assert
/// wrapper is compiled out entirely on release.
#[warn(unused_macros)]
macro_rules! error_guarded {
    (
        $vis:vis fn $name:ident($($arg:ident : $ty:ty),*) -> $ret:ty $body:block,
        allowed: $allowed:pat
    ) => {
        #[cfg(not(debug_assertions))]
        $vis fn $name($($arg: $ty),*) -> $ret { $body }

        #[cfg(debug_assertions)]
        $vis fn $name($($arg: $ty),*) -> $ret {
            fn __guarded_impl($($arg: $ty),*) -> $ret { $body }
            let __result = __guarded_impl($($arg),*);
            debug_assert!(
                match &__result {
                    Ok(_) => true,
                    Err(e) => matches!(e, $allowed),
                },
                "{} returned a disallowed error variant: {:?}",
                stringify!($name),
                __result.as_ref().err()
            );
            __result
        }
    };
}

/// Panic-on-error extension for any `Result<T, E>` whose error is displayable.
///
/// Use it opt-in at the call site where an error *is* a process-abort reason:
///
/// ```rust
/// use tar_dedup_lib::error::{Error, ToPanic};
///
/// fn fallible() -> Result<(), Error> { Ok(()) }
///
/// fn run() -> Result<(), Error> {
///     // `to_panic()` aborts the process on `Err` under `--features
///     // debug-err-to-panic`; without the feature it is a pass-through and `?`
///     // simply propagates the error to the caller.
///     fallible().to_panic()?;
///     Ok(())
/// }
///
/// assert!(run().is_ok());
/// ```
///
/// With the `debug-err-to-panic` feature enabled `.to_panic()` converts the `Err`
/// into a panic (with `RUST_BACKTRACE=1` you get the origin's call stack);
/// without it the result passes through untouched and `?` propagates as usual.
/// Errors that are handled locally (per-file recording into the DB, expected
/// test outcomes, ...) are never affected unless you place `.to_panic()?`.
pub trait ToPanic<T, E>: Sized {
    fn to_panic(self) -> std::result::Result<T, E>;
}

#[cfg(not(feature = "debug-err-to-panic"))]
impl<T, E> ToPanic<T, E> for std::result::Result<T, E> {
    #[inline]
    fn to_panic(self) -> std::result::Result<T, E> {
        self
    }
}

#[cfg(feature = "debug-err-to-panic")]
impl<T, E: std::fmt::Display> ToPanic<T, E> for std::result::Result<T, E> {
    #[inline]
    #[track_caller]
    fn to_panic(self) -> std::result::Result<T, E> {
        match self {
            Ok(v) => Ok(v),
            Err(e) => panic!("{e}"),
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("{0}")]
    FileStat(#[from] FileStatError),

    #[error("invalid configuration: {0}")]
    Config(String),

    #[error("interrupted")]
    Interrupted,

    #[error("{0}")]
    Other(#[from] anyhow::Error),

   // #[error("{0}")]
   // Malformatted(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        if source.kind() == std::io::ErrorKind::Interrupted {
            return Self::Interrupted;
        }
        Self::FileStat(FileStatError::Io {
            path: path.into(),
            source,
        })
    }

    pub fn copy_io(path: impl Into<PathBuf>, source: &std::io::Error) -> Self {
        if source.kind() == std::io::ErrorKind::Interrupted {
            return Self::Interrupted;
        }
        Self::FileStat(FileStatError::copy_io(path.into(), source))
    }

    /// The path an erring filesystem operation was about, when `FileStat`.
    pub fn io_path(&self) -> Option<PathBuf> {
        match self {
            Self::FileStat(fse) => fse.io_path(),
            _ => None,
        }
    }

    /// Convert an `Error` into a [`FileStatError`] for the persistent error log.
    /// `fallback` is used as the path when the error has none (non-`FileStat`
    /// variants). A carried [`FileStatError`] is recreated as faithfully as
    /// possible (OS errors round-trip through the raw code).
    pub fn to_file_stat(&self, fallback: Option<&Path>) -> FileStatError {
        match self {
            Self::FileStat(fse) => fse.recreate(),
            Self::Database(e) => FileStatError::General {
                path: fallback.map(|p| p.to_path_buf()),
                message: format!("database error: {e}"),
            },
            Self::Config(m) => FileStatError::General {
                path: fallback.map(|p| p.to_path_buf()),
                message: format!("invalid configuration: {m}"),
            },
            Self::Interrupted => FileStatError::General {
                path: fallback.map(|p| p.to_path_buf()),
                message: "interrupted".to_string(),
            },
            Self::Other(e) => FileStatError::General {
                path: fallback.map(|p| p.to_path_buf()),
                message: e.to_string(),
            },
        }
    }

    /// Convert an `Error` into a [`FileStatError`] for the persistent error log.
    /// If the variant is not FileStatError, we drop to None
    pub fn to_only_file_stat(&self) -> Option<FileStatError> {
        match self {
            Self::FileStat(fse) => Some(fse.recreate()),
            _ => None,
        }
    }
}

/// Rebuild an `io::Error` from a reference. Real OS errors round-trip through
/// the raw code (kind and message are both recovered by
/// `from_raw_os_error`); synthetic errors without a code keep kind + message.
fn rebuilt(source: &std::io::Error) -> std::io::Error {
    match source.raw_os_error() {
        Some(code) => std::io::Error::from_raw_os_error(code),
        None => std::io::Error::new(source.kind(), source.to_string()),
    }
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        if source.kind() == std::io::ErrorKind::Interrupted {
            Self::Interrupted
        } else {
            Self::FileStat(FileStatError::Io {
                path: PathBuf::new(),
                source,
            })
        }
    }
}

pub type FileStatResult<T> = std::result::Result<T, FileStatError>;

/// Error Wrapper to capture all Error Types encountered while performing inventory and file system
/// operations. Error Buffer needs a common type to pack errors into
#[derive(Debug, thiserror::Error)]
pub enum FileStatError {
    #[error("io error at {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },

    #[error("json error at {path}: {source}")]
    Json { path: PathBuf, source: serde_json::Error },

    #[error("xattr error at {path}: {source}")]
    Xattrs { path: PathBuf, source: xattrs::Error },

    #[error("acl error at {path}: {source}")]
    PosixAcl { path: PathBuf, source: posix_acl::ACLError },

    #[error("selinux error at {path}: {source}")]
    SELinux { path: PathBuf, source: selinux::errors::Error },

    #[error("nix (syscall) error at {path}: {source}")]
    Nix { path: PathBuf, source: nix::Error },

    #[error("posix qualfier parse error at {path}: {source}")]
    PosixQualifierParser { path: PathBuf, source: PosixQualifierParserError},

    #[error("bBase64 decoding error at {path}: {source}")]
    Base64DecodingError { path: PathBuf, source: base64::DecodeError},

    #[error("materialize conflict at {path}: {details}")]
    MaterializeConflict { path: PathBuf, details: String },

    #[error("general error at {path:?}: {message}")]
    General { path: Option<PathBuf>, message: String },
}

impl FileStatError {
    pub fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io { path: path.to_path_buf(), source }
    }
    pub fn copy_io(path: impl Into<PathBuf>, source: &std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source: std::io::Error::new(source.kind(), source.to_string())
        }
    }

    pub fn json(path: &Path, source: serde_json::Error) -> Self {
        Self::Json { path: path.to_path_buf(), source }
    }
    pub fn xattrs(path: &Path, source: xattrs::Error) -> Self {
        Self::Xattrs { path: path.to_path_buf(), source }
    }
    pub fn posix_acl(path: &Path, source: posix_acl::ACLError) -> Self {
        Self::PosixAcl { path: path.to_path_buf(), source }
    }
    pub fn selinux(path: &Path, source: selinux::errors::Error) -> Self {
        Self::SELinux { path: path.to_path_buf(), source }
    }
    pub fn posix_qualifier_parser(path: &Path, source: PosixQualifierParserError)
        -> Self {
        Self::PosixQualifierParser {path: path.to_path_buf(), source}
    }
    pub fn nix(path: &Path, source: nix::Error) -> Self {
        Self::Nix { path: path.to_path_buf(), source }
    }
    pub fn general(path: Option<&Path>, message: String) -> Self {
        Self::General { path: path.map(|p| p.to_path_buf()), message }
    }

    /// The path this error is about, `None` when absent (`General` may carry none).
    pub fn io_path(&self) -> Option<PathBuf> {
        match self {
            Self::General { path, .. } => path.clone(),
            Self::Io { path, .. }
            | Self::Json { path, .. }
            | Self::Xattrs { path, .. }
            | Self::PosixAcl { path, .. }
            | Self::SELinux { path, .. }
            | Self::Nix { path, .. }
            | Self::PosixQualifierParser { path, .. }
            | Self::Base64DecodingError { path, .. }
            | Self::MaterializeConflict { path, .. } => Some(path.clone()),
        }
    }

    /// Recreate an owned [`FileStatError`] from a reference. `Io` errors
    /// round-trip through the raw OS code (see [`rebuilt`]); nothing is lost
    /// that the persistent error log stores (kind + message).
    /// General is recreated as is,
    /// Any other error is converted to a GeneralError
    pub fn recreate(&self) -> FileStatError {
        match self {
            Self::Io { path, source } => Self::Io {
                path: path.clone(),
                source: rebuilt(source),
            },
            Self::General { path, message } => Self::General {
                path: path.clone(),
                message: message.clone(),
            },
            other => Self::General {
                path: other.io_path(),
                message: other.to_string(),
            },
        }
    }

    /// Human-readable discriminator (and, where available, underlying detail) of the
    /// error, e.g. `"Io/PermissionDenied"` or `"Nix/EACCES"` — stored as `error_type`.
    pub fn kind(&self) -> String {
        match self {
            Self::Io { source, .. } => format!("Io/{:?}", source.kind()),
            Self::Json { .. } => "Json".to_string(),
            Self::Xattrs { .. } => "Xattrs".to_string(),
            Self::PosixAcl { .. } => "PosixAcl".to_string(),
            Self::SELinux { .. } => "SELinux".to_string(),
            Self::Nix { source, .. } => format!("Nix/{}", source),
            Self::PosixQualifierParser { .. } => "PosixQualifierParser".to_string(),
            Self::Base64DecodingError { .. } => "Base64DecodingError".to_string(),
            Self::MaterializeConflict { .. } => "MaterializeConflict".to_string(),
            Self::General { .. } => "General".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// TEST
// ---------------------------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn fixture_path() -> PathBuf {
        PathBuf::from("/data/example.txt")
    }

    fn eacces() -> std::io::Error {
        std::io::Error::from_raw_os_error(13) // EACCES -> PermissionDenied
    }

    fn interrupted() -> std::io::Error {
        std::io::Error::from(std::io::ErrorKind::Interrupted)
    }

    fn sample_error_variants() -> Vec<FileStatError> {
        let path = fixture_path();
        vec![
            FileStatError::Io { path: path.clone(), source: eacces() },
            FileStatError::Json { path: path.clone(), source: serde_json::from_str::<u32>("boom").unwrap_err() },
            FileStatError::Xattrs { path: path.clone(), source: xattrs::list_xattr("/definitely-missing-xattr-path").expect_err("missing path") },
            FileStatError::PosixAcl { path: path.clone(), source: posix_acl::PosixACL::read_acl("/definitely-missing-acl-path").expect_err("missing path") },
            FileStatError::SELinux { path: path.clone(), source: selinux::errors::Error::PathIsInvalid(path.clone()) },
            FileStatError::Nix { path: path.clone(), source: nix::errno::Errno::EPERM },
            FileStatError::PosixQualifierParser { path: path.clone(), source: crate::common::xattr::PosixQualifierParserError::UnexpectedString("x".into()) },
            FileStatError::Base64DecodingError { path: path.clone(), source: base64::engine::general_purpose::STANDARD.decode("!!not-base64!!").expect_err("invalid base64") },
            FileStatError::MaterializeConflict { path: path.clone(), details: "two members map here".into() },
            FileStatError::General { path: Some(path), message: "message".into() },
        ]
    }

    #[test]
    fn file_stat_variants_display_and_report_paths() {
        for e in sample_error_variants() {
            assert_eq!(e.io_path(), Some(fixture_path()), "io_path of {e:?}");
            let displayed = e.to_string();
            assert!(
                displayed.contains("example.txt"),
                "display must carry the path: {displayed}"
            );
            assert!(!e.kind().is_empty(), "kind of {:?}", e);
        }
    }

    #[test]
    fn file_stat_kind_classifies_variants() {
        let path = fixture_path();
        let io = FileStatError::io(&path, eacces());
        assert_eq!(io.kind(), "Io/PermissionDenied");

        let json = FileStatError::json(&path, serde_json::from_str::<u32>("boom").unwrap_err());
        assert_eq!(json.kind(), "Json");
        let general = FileStatError::general(None, "m".into());
        assert_eq!(general.kind(), "General");
        assert_eq!(general.io_path(), None, "General may carry no path");
        let nix = FileStatError::nix(&path, nix::errno::Errno::EPERM);
        assert_eq!(nix.kind(), "Nix/EPERM: Operation not permitted");
        let conflict = FileStatError::MaterializeConflict { path, details: "d".into() };
        assert_eq!(conflict.kind(), "MaterializeConflict");
    }

    #[test]
    fn general_error_round_trips_through_recreate() {
        let path = fixture_path();
        let err = FileStatError::general(Some(&path), "hello".into());
        let re = err.recreate();
        match re {
            FileStatError::General { path: p, message } => {
                assert_eq!(p.as_deref(), Some(fixture_path().as_path()));
                assert_eq!(message, "hello");
            }
            other => panic!("expected General, got {other:?}"),
        }
    }

    #[test]
    fn io_errors_round_trip_through_recreate_by_raw_os_code() {
        let path = fixture_path();
        let err = FileStatError::io(&path, eacces());
        let re = err.recreate();
        assert!(matches!(re, FileStatError::Io { .. }), "{:?}", re);
        // Real OS errors keep their kind through the raw code round-trip.
        assert_eq!(re.kind(), "Io/PermissionDenied");
    }

    #[test]
    fn non_io_variants_recreate_as_general() {
        for e in sample_error_variants() {
            let re = e.recreate();
            assert!(
                matches!(re, FileStatError::General { .. }) || re.kind() == "Io/PermissionDenied",
                "{e:?} -> {re:?}"
            );
            assert_eq!(re.io_path(), e.io_path(), "path preserved for {e:?}");
        }
    }

    #[test]
    fn io_and_copy_io_map_interrupted_to_interrupted() {
        let path = fixture_path();
        assert!(matches!(Error::io(&path, interrupted()), Error::Interrupted));
        assert!(matches!(Error::copy_io(&path, &interrupted()), Error::Interrupted));
        let io = Error::io(&path, eacces());
        assert_eq!(io.to_string(), "io error at /data/example.txt: Permission denied (os error 13)");
        assert_eq!(io.io_path(), Some(path));
        assert!(matches!(io, Error::FileStat(FileStatError::Io { .. })));
    }

    #[test]
    fn from_io_maps_interrupted_and_others() {
        assert!(matches!(Error::from(interrupted()), Error::Interrupted));
        let e = Error::from(eacces());
        assert!(matches!(e, Error::FileStat(FileStatError::Io { .. })));
        assert_eq!(e.io_path(), Some(PathBuf::new()), "empty path for From<io::Error>");
    }

    #[test]
    fn io_path_none_for_non_file_variants() {
        assert_eq!(Error::Config("x".into()).io_path(), None);
        assert_eq!(Error::Interrupted.io_path(), None);
        assert_eq!(Error::Other(anyhow::anyhow!("boom")).io_path(), None);
        // A database error (open garbage bytes as a sqlite file).
        let dir = tempfile::tempdir().expect("tempdir");
        let bad = dir.path().join("bad.sqlite");
        std::fs::write(&bad, b"this is not a sqlite database").expect("write garbage");
        let db_err = crate::db::Database::open(&bad).err().expect("garbage bytes must fail");
        assert!(matches!(db_err, Error::Database(_)));
        assert_eq!(db_err.io_path(), None);
    }

    #[test]
    fn to_file_stat_falls_back_to_general() {
        let path = fixture_path();
        for (err, needle) in [
            (Error::Config("cfg".into()), "invalid configuration: cfg"),
            (Error::Other(anyhow::anyhow!("boom")), "boom"),
            (Error::Interrupted, "interrupted"),
        ] {
            let fse = err.to_file_stat(Some(&path));
            assert!(matches!(fse, FileStatError::General { .. }), "{err:?}");
            assert_eq!(fse.io_path().as_deref(), Some(path.as_path()));
            assert!(fse.to_string().contains(needle), "{fse:?} vs {needle}");
            assert!(err.to_only_file_stat().is_none(), "{err:?}");
        }
    }

    #[test]
    fn to_file_stat_preserves_carried_file_stat() {
        let path = fixture_path();
        let carried = FileStatError::io(&path, eacces());
        let fse = Error::FileStat(carried).to_file_stat(None);
        assert!(matches!(fse, FileStatError::Io { .. }), "{fse:?}");
        let carried2 = FileStatError::io(&path, eacces());
        let kept = Error::FileStat(carried2).to_only_file_stat();
        assert!(kept.is_some());
        assert_eq!(kept.unwrap().kind(), "Io/PermissionDenied");
    }
}
