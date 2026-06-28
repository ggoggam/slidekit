use std::path::PathBuf;

/// Errors returned by OpenSlide operations.
#[derive(Debug, thiserror::Error)]
pub enum OpenSlideError {
    #[error("unsupported format: {path}")]
    UnsupportedFormat { path: PathBuf },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[cfg(feature = "object-store")]
    #[error("object store error: {0}")]
    ObjectStore(#[from] object_store::Error),

    #[error("invalid level {level}")]
    InvalidLevel { level: i32 },

    #[error("invalid argument: {0}")]
    InvalidArgument(String),

    #[error("decode error: {0}")]
    Decode(String),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, OpenSlideError>;
