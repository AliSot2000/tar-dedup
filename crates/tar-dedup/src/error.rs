use std::path::PathBuf;

use thiserror::Error;

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
