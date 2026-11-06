use std::path::PathBuf;

/// Errors surfaced across crate boundaries.
///
/// Each variant names the subsystem that failed so the UI can present a useful
/// message instead of a generic "something went wrong".
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("database error: {0}")]
    Database(String),

    #[error("could not read metadata for {path}: {message}")]
    Metadata { path: PathBuf, message: String },

    #[error("invalid configuration: {0}")]
    Config(String),

    #[error("artwork error: {0}")]
    Artwork(String),

    #[error("playback error: {0}")]
    Playback(String),

    #[error("device error: {0}")]
    Device(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }

    pub fn metadata(path: impl Into<PathBuf>, message: impl Into<String>) -> Self {
        Error::Metadata {
            path: path.into(),
            message: message.into(),
        }
    }

    pub fn artwork(message: impl Into<String>) -> Self {
        Error::Artwork(message.into())
    }

    pub fn playback(message: impl Into<String>) -> Self {
        Error::Playback(message.into())
    }

    pub fn device(message: impl Into<String>) -> Self {
        Error::Device(message.into())
    }
}

pub type Result<T> = std::result::Result<T, Error>;
