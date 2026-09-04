//! Error and result types for Arkstore.

use thiserror::Error;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, ArkError>;

/// Conventional process exit code for a user interrupt (SIGINT).
pub const EXIT_INTERRUPTED: u8 = 130;

/// Every error Arkstore surfaces. Known, expected failures get a clean message;
/// unexpected ones bubble up with their source. Messages that may carry
/// credentials are redacted (see [`crate::redact`]) before construction.
#[derive(Debug, Error)]
pub enum ArkError {
    /// The configuration is missing, unreadable, or malformed.
    #[error("configuration error: {0}")]
    Config(String),

    /// A configuration value failed validation; the message names the field.
    #[error("invalid configuration: {0}")]
    Validation(String),

    /// Secrets could not be loaded or applied.
    #[error("secrets error: {0}")]
    Secrets(String),

    /// An archive manifest is missing, malformed, or inconsistent.
    #[error("manifest error: {0}")]
    Manifest(String),

    /// The object store could not be reached or a request failed.
    #[error("object store error: {0}")]
    Store(String),

    /// An engine backend failed. The message is already redacted.
    #[error("{engine}: {message}")]
    Engine {
        engine: &'static str,
        message: String,
    },

    /// An engine was requested that this binary was not compiled with.
    #[error(
        "{engine} support was not built into this binary; \
         rebuild with `--features {feature}` or use a full release"
    )]
    EngineNotBuilt {
        engine: &'static str,
        feature: &'static str,
    },

    /// A safety guard refused to proceed (never-production, non-empty target).
    #[error("refusing to proceed: {0}")]
    Refused(String),

    /// The run was interrupted by the user.
    #[error("interrupted")]
    Interrupted,

    /// A failure inside Arkstore itself (a worker task died, an invariant
    /// broke) — a bug, not an operator error.
    #[error("internal error: {0}")]
    Internal(String),

    /// A code path that is planned but not yet implemented.
    #[error("not yet implemented: {0}")]
    NotImplemented(&'static str),

    /// An underlying I/O failure.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A YAML (de)serialization failure.
    #[error("yaml error: {0}")]
    Yaml(#[from] serde_yaml::Error),

    /// A JSON (de)serialization failure.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl ArkError {
    /// The process exit code this error maps to: `130` for an interrupt,
    /// `1` for everything else.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Interrupted => EXIT_INTERRUPTED,
            _ => 1,
        }
    }
}
