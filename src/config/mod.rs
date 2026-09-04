//! The declarative Arkstore configuration: global policy, sources, targets.
//!
//! Three files (PRD §7): `arkstore.yaml` (policy; may inline `sources` /
//! `targets`), plus optional sibling `sources.yaml` and `targets.yaml` used
//! when the inline lists are empty. Secrets are merged in afterwards
//! ([`crate::secrets`]).

mod concurrency;
pub mod name;
mod policy;
mod source;
mod target;

pub use concurrency::{Concurrency, Limit, Resolved as ResolvedConcurrency};
pub use policy::{ArchivePolicy, CleanupPolicy, RetentionTiers};
pub use source::{ArchiveRule, CopyFormat, Source, SourceType, TlsMode};
pub use target::{
    check_not_production, resolve_target, EnvLookup, ProcessEnv, ResolvedTarget, Target,
    TargetOverrides, ENV_TARGET, ENV_TARGET_DB, ENV_TARGET_HOST, ENV_TARGET_PASSWORD,
    ENV_TARGET_PATH, ENV_TARGET_PORT, ENV_TARGET_USER,
};

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono_tz::Tz;
use serde::de::DeserializeOwned;
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
    /// IANA timezone name used for all calendar decisions (retention, archive,
    /// backup stamps).
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

/// Where credentials come from (PRD §8).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SecretsConfig {
    /// A local secrets YAML file (dev / self-hosted). Overridden by
    /// `ARKSTORE_SECRETS_FILE`.
    pub file: Option<PathBuf>,
}

/// The `restore:` block: an optional inline target used when no `targets`
/// entry matches.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RestoreConfig {
    pub target: Option<Target>,
}

/// The `verify:` block: the server on which Arkstore may create a throwaway
/// database (`arkstore_verify_<source>_<stamp>`), dropped after the run.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VerifyConfig {
    pub server: Option<Target>,
}

/// The full configuration.
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
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub restore: RestoreConfig,
    #[serde(default)]
    pub verify: VerifyConfig,
    #[serde(default)]
    pub sources: Vec<Source>,
    #[serde(default)]
    pub targets: Vec<Target>,
}

#[derive(Deserialize)]
struct SourcesFile {
    #[serde(default)]
    sources: Vec<Source>,
}

#[derive(Deserialize)]
struct TargetsFile {
    #[serde(default)]
    targets: Vec<Target>,
}

impl Config {
    /// Read `arkstore.yaml` and pull in sibling `sources.yaml` /
    /// `targets.yaml` when the inline lists are empty — **without**
    /// validating. Callers apply CLI overrides and merge secrets first, then
    /// call [`Config::validate`], so a `host` or `user` that lives only in the
    /// secrets file, or a `--timezone` override, is in place before checks run.
    pub fn load_unvalidated(path: &Path) -> Result<Self> {
        let mut config: Config = read_yaml(path)?;
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        if config.sources.is_empty() {
            if let Some(file) = read_sibling::<SourcesFile>(dir, "sources.yaml")? {
                config.sources = file.sources;
            }
        }
        if config.targets.is_empty() {
            if let Some(file) = read_sibling::<TargetsFile>(dir, "targets.yaml")? {
                config.targets = file.targets;
            }
        }
        Ok(config)
    }

    /// [`Config::load_unvalidated`] followed by [`Config::validate`] — for callers with
    /// no overrides or secrets to merge.
    pub fn load(path: &Path) -> Result<Self> {
        let config = Self::load_unvalidated(path)?;
        config.validate()?;
        Ok(config)
    }

    /// Reject configurations that would fail an operation up front.
    pub fn validate(&self) -> Result<()> {
        if self.aws.bucket.trim().is_empty() {
            return Err(ArkError::Validation("aws.bucket must not be empty".into()));
        }
        if self.aws.region.trim().is_empty() {
            return Err(ArkError::Validation("aws.region must not be empty".into()));
        }
        self.timezone()?;
        let mut seen = HashSet::new();
        for source in &self.sources {
            source.validate()?;
            if !seen.insert(source.name.as_str()) {
                return Err(ArkError::Validation(format!(
                    "duplicate source name `{}`",
                    source.name
                )));
            }
        }
        let mut seen = HashSet::new();
        for target in self.targets.iter().chain(self.restore.target.iter()) {
            target.validate()?;
            if !seen.insert(target.name.as_str()) {
                return Err(ArkError::Validation(format!(
                    "duplicate target name `{}`",
                    target.name
                )));
            }
        }
        Ok(())
    }

    /// The configured timezone, parsed.
    pub fn timezone(&self) -> Result<Tz> {
        self.app.timezone.parse::<Tz>().map_err(|_| {
            ArkError::Validation(format!(
                "app.timezone `{}` is not a valid IANA timezone",
                self.app.timezone
            ))
        })
    }

    /// Enabled sources, optionally narrowed to one engine type and/or one name.
    pub fn selected_sources(&self, kind: Option<SourceType>, only: Option<&str>) -> Vec<&Source> {
        self.sources
            .iter()
            .filter(|s| s.enable)
            .filter(|s| kind.is_none_or(|k| s.source_type == k))
            .filter(|s| only.is_none_or(|name| s.name == name))
            .collect()
    }

    /// A single source by name (enabled or not).
    pub fn source(&self, name: &str) -> Result<&Source> {
        self.sources
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| ArkError::Validation(format!("no source named `{name}`")))
    }
}

fn read_yaml<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| ArkError::Config(format!("cannot read {}: {e}", path.display())))?;
    serde_yaml::from_str(&text).map_err(|e| ArkError::Config(format!("{}: {e}", path.display())))
}

fn read_sibling<T: DeserializeOwned>(dir: &Path, file: &str) -> Result<Option<T>> {
    let path = dir.join(file);
    if !path.is_file() {
        return Ok(None);
    }
    read_yaml(&path).map(Some)
}
