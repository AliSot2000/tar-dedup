use crate::common::xattr::PosixQualifierParserError;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

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

    pub fn is_interrupted(&self) -> bool {
        matches!(self, Self::Interrupted)
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
            _ => None
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

    #[error("general error at {path:?}: {message}")]
    General { path: Option<PathBuf>, message: String },
}

impl FileStatError {
    pub fn io(path: &Path, source: std::io::Error) -> Self {
        Self::Io { path: path.to_path_buf(), source }
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
            | Self::Base64DecodingError { path, .. } => Some(path.clone()),
        }
    }

    /// Recreate an owned [`FileStatError`] from a reference. `Io` errors
    /// round-trip through the raw OS code (see [`rebuilt`]); nothing is lost
    /// that the persistent error log stores (kind + message).
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
            Self::Io { source, .. } => match source.kind() {
                std::io::ErrorKind::Other => "Io/Other".to_string(),
                std::io::ErrorKind::PermissionDenied => "Io/PermissionDenied".to_string(),
                std::io::ErrorKind::NotFound => "Io/NotFound".to_string(),
                std::io::ErrorKind::AlreadyExists => "Io/AlreadyExists".to_string(),
                std::io::ErrorKind::InvalidInput => "Io/InvalidInput".to_string(),
                std::io::ErrorKind::Unsupported => "Io/Unsupported".to_string(),
                other => format!("Io/{}", other),
            },
            Self::Json { .. } => "Json".to_string(),
            Self::Xattrs { .. } => "Xattrs".to_string(),
            Self::PosixAcl { .. } => "PosixAcl".to_string(),
            Self::SELinux { .. } => "SELinux".to_string(),
            Self::Nix { source, .. } => format!("Nix/{}", source),
            Self::PosixQualifierParser { .. } => "PosixQualifierParser".to_string(),
            Self::Base64DecodingError { .. } => "Base64DecodingError".to_string(),
            Self::General { .. } => "General".to_string(),
        }
    }
}