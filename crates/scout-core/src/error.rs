use std::path::PathBuf;

/// The single error type crossing scout's port boundaries.
///
/// Adapters map their own failures into these variants so front-ends can report them
/// uniformly. Variants carry enough context (paths, shard names) to debug without
/// re-running.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("corrupt shard {shard}: {reason}")]
    Corrupt { shard: String, reason: String },
    #[error("invalid pattern: {0}")]
    Pattern(String),
    #[error("{context}: {source}")]
    Backend {
        context: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io { path: path.into(), source }
    }

    pub fn backend(context: impl Into<String>, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Backend { context: context.into(), source: Box::new(source) }
    }
}
