use std::path::PathBuf;
use thiserror::Error;
use crate::common::xattr::PosixQualifierParserError;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },

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
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub fn is_interrupted(&self) -> bool {
        matches!(self, Self::Interrupted)
    }
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        if source.kind() == std::io::ErrorKind::Interrupted {
            Self::Interrupted
        } else {
            Self::Io {
                path: PathBuf::new(),
                source,
            }
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
    pub fn io(path: &std::path::Path, source: std::io::Error) -> Self {
        Self::Io { path: path.to_path_buf(), source }
    }
    pub fn json(path: &std::path::Path, source: serde_json::Error) -> Self {
        Self::Json { path: path.to_path_buf(), source }
    }
    pub fn xattrs(path: &std::path::Path, source: xattrs::Error) -> Self {
        Self::Xattrs { path: path.to_path_buf(), source }
    }
    pub fn posix_acl(path: &std::path::Path, source: posix_acl::ACLError) -> Self {
        Self::PosixAcl { path: path.to_path_buf(), source }
    }
    pub fn selinux(path: &std::path::Path, source: selinux::errors::Error) -> Self {
        Self::SELinux { path: path.to_path_buf(), source }
    }
    pub fn posix_qualifier_parser(path: &std::path::Path, source: PosixQualifierParserError) 
        -> Self {
        Self::PosixQualifierParser {path: path.to_path_buf(), source}
    }
    pub fn nix(path: &std::path::Path, source: nix::Error) -> Self {
        Self::Nix { path: path.to_path_buf(), source }
    }
    pub fn general(path: Option<&std::path::Path>, message: String) -> Self {
        Self::General { path: path.map(|p| p.to_path_buf()), message }
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