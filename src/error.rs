//! Error and result types for Arkstore.

use thiserror::Error;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, ArkError>;

/// Every error Arkstore surfaces. Known, expected failures get a clean message;
/// unexpected ones bubble up with their source.
#[derive(Debug, Error)]
pub enum ArkError {
    /// The configuration is missing, unreadable, or invalid.
    #[error("configuration error: {0}")]
    Config(String),

    /// The object store could not be reached or a request failed.
    #[error("object store error: {0}")]
    Store(String),

    /// An engine was requested that this binary was not compiled with.
    #[error(
        "{engine} support was not built into this binary; \
         rebuild with `--features {feature}` or use a full release"
    )]
    EngineNotBuilt {
        engine: &'static str,
        feature: &'static str,
    },

    /// A code path that is planned but not yet implemented.
    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),

    /// An underlying I/O failure.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A YAML (de)serialization failure.
    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),
}
