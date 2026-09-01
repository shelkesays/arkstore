//! The declarative Arkstore configuration: global policy plus sources.

mod concurrency;
mod policy;
mod source;

pub use concurrency::{Concurrency, Limit, Resolved as ResolvedConcurrency};
pub use policy::{ArchivePolicy, CleanupPolicy, RetentionTiers};
pub use source::{ArchiveRule, Source, SourceType};

use std::path::Path;

use serde::Deserialize;

use crate::error::{ArkError, Result};

/// Object-store / AWS settings.
#[derive(Debug, Clone, Deserialize)]
pub struct AwsConfig {
    pub bucket: String,
    pub region: String,
    /// Top-level prefix for backups. Cleanup scans only under this prefix.
    #[serde(default = "default_folder")]
    pub folder: String,
    /// Custom endpoint for S3-compatible stores (e.g. MinIO). `None` = AWS S3.
    #[serde(default)]
    pub endpoint: Option<String>,
}

fn default_folder() -> String {
    "arkstore".to_string()
}

/// App-wide settings.
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    /// IANA timezone name used for all calendar decisions (retention, archive).
    #[serde(default = "default_timezone")]
    pub timezone: String,
    /// Default log level when neither `--log-level` nor `RUST_LOG` is set.
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

fn default_timezone() -> String {
    "UTC".to_string()
}

fn default_log_level() -> String {
    "info".to_string()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            timezone: default_timezone(),
            log_level: default_log_level(),
        }
    }
}

/// The full configuration: global policy plus the source list.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub app: AppConfig,
    pub aws: AwsConfig,
    #[serde(default)]
    pub cleanup: CleanupPolicy,
    #[serde(default)]
    pub archive: ArchivePolicy,
    #[serde(default)]
    pub concurrency: Concurrency,
    #[serde(default)]
    pub sources: Vec<Source>,
}

impl Config {
    /// Load and validate configuration from a YAML file.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ArkError::Config(format!("cannot read {}: {e}", path.display())))?;
        let config: Config = serde_yaml::from_str(&text)?;
        config.validate()?;
        Ok(config)
    }

    /// Reject configurations that would fail an operation up front.
    fn validate(&self) -> Result<()> {
        if self.aws.bucket.trim().is_empty() {
            return Err(ArkError::Config("aws.bucket must not be empty".into()));
        }
        if self.aws.region.trim().is_empty() {
            return Err(ArkError::Config("aws.region must not be empty".into()));
        }
        Ok(())
    }

    /// Enabled sources, optionally narrowed to a single name.
    pub fn selected_sources(&self, only: Option<&str>) -> Vec<&Source> {
        self.sources
            .iter()
            .filter(|s| s.enable)
            .filter(|s| only.is_none_or(|name| s.name == name))
            .collect()
    }
}
