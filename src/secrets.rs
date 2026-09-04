//! Secrets: credentials never live in the tracked config. They are merged
//! into sources / targets at load time from a local secrets file (dev /
//! self-hosted) — a secrets-manager provider follows — and held in zeroizing
//! memory. Nothing here ever touches `argv`, a child environment, or a temp
//! file (PRD §8).

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::{info, warn};
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::config::{Config, EnvLookup};
use crate::error::{ArkError, Result};

/// Environment variable pointing at a local secrets file.
pub const ENV_SECRETS_FILE: &str = "ARKSTORE_SECRETS_FILE";

/// A credential held in memory that is zeroed on drop and never printed.
#[derive(Clone, PartialEq, Eq, Deserialize, Zeroize, ZeroizeOnDrop)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    /// Borrow the secret value. Keep the borrow short; never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

/// A source of credentials that hydrates a [`Config`] in place.
pub trait SecretsProvider {
    fn hydrate(&self, config: &mut Config) -> Result<()>;
}

/// Credentials are already present in the config (or not needed).
pub struct InlineSecrets;

impl SecretsProvider for InlineSecrets {
    fn hydrate(&self, _config: &mut Config) -> Result<()> {
        Ok(())
    }
}

/// One entry in the secrets file. Connection fields other than the password
/// are optional overrides (a prod host may live only in the secret).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct SecretEntry {
    password: Option<Secret>,
    user: Option<String>,
    host: Option<String>,
    port: Option<u16>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct SecretsFile {
    sources: BTreeMap<String, SecretEntry>,
    targets: BTreeMap<String, SecretEntry>,
}

/// A local YAML secrets file keyed by source / target name:
///
/// ```yaml
/// sources:
///   appdb: { password: "…", host: db.internal }   # host/user/port optional
/// targets:
///   staging: { password: "…" }
/// ```
pub struct LocalFileSecrets {
    path: PathBuf,
}

impl LocalFileSecrets {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

/// Mutable view of the connection fields a secret entry may fill in.
struct ConnFields<'a> {
    host: &'a mut Option<String>,
    port: &'a mut Option<u16>,
    user: &'a mut Option<String>,
    password: &'a mut Option<Secret>,
}

impl SecretEntry {
    fn apply(&self, fields: ConnFields<'_>) {
        if self.password.is_some() {
            *fields.password = self.password.clone();
        }
        if self.user.is_some() {
            *fields.user = self.user.clone();
        }
        if self.host.is_some() {
            *fields.host = self.host.clone();
        }
        if self.port.is_some() {
            *fields.port = self.port;
        }
    }
}

fn read_secrets_file(path: &Path) -> Result<SecretsFile> {
    warn_if_world_readable(path);
    let text = std::fs::read_to_string(path)
        .map_err(|e| ArkError::Secrets(format!("cannot read {}: {e}", path.display())))?;
    serde_yaml::from_str(&text).map_err(|e| ArkError::Secrets(format!("{}: {e}", path.display())))
}

fn hydrate_source(config: &mut Config, name: &str, entry: &SecretEntry) {
    match config.sources.iter_mut().find(|s| s.name == name) {
        Some(s) => entry.apply(ConnFields {
            host: &mut s.host,
            port: &mut s.port,
            user: &mut s.user,
            password: &mut s.password,
        }),
        None => warn!(name, "secrets file names a source that is not configured"),
    }
}

fn hydrate_target(config: &mut Config, name: &str, entry: &SecretEntry) {
    let found = config
        .targets
        .iter_mut()
        .chain(config.restore.target.iter_mut())
        .chain(config.verify.server.iter_mut())
        .find(|t| t.name == name);
    match found {
        Some(t) => entry.apply(ConnFields {
            host: &mut t.host,
            port: &mut t.port,
            user: &mut t.user,
            password: &mut t.password,
        }),
        None => warn!(name, "secrets file names a target that is not configured"),
    }
}

impl SecretsProvider for LocalFileSecrets {
    fn hydrate(&self, config: &mut Config) -> Result<()> {
        let file = read_secrets_file(&self.path)?;
        for (name, entry) in &file.sources {
            hydrate_source(config, name, entry);
        }
        for (name, entry) in &file.targets {
            hydrate_target(config, name, entry);
        }
        info!(
            path = %self.path.display(),
            sources = file.sources.len(),
            targets = file.targets.len(),
            "secrets loaded"
        );
        Ok(())
    }
}

/// Pick the provider: `ARKSTORE_SECRETS_FILE`, else `secrets.file` in the
/// config, else inline.
pub fn provider_for(config: &Config, env: &dyn EnvLookup) -> Box<dyn SecretsProvider> {
    match env
        .get(ENV_SECRETS_FILE)
        .map(PathBuf::from)
        .or_else(|| config.secrets.file.clone())
    {
        Some(path) => Box::new(LocalFileSecrets::new(path)),
        None => Box::new(InlineSecrets),
    }
}

/// Hydrate `config` with the configured provider.
pub fn load_secrets(config: &mut Config, env: &dyn EnvLookup) -> Result<()> {
    provider_for(config, env).hydrate(config)
}

#[cfg(unix)]
fn warn_if_world_readable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) if meta.permissions().mode() & 0o044 != 0 => {
            warn!(path = %path.display(), "secrets file is readable by group/others; consider chmod 600");
        }
        Ok(_) => {}
        Err(err) => warn!(path = %path.display(), %err, "cannot stat secrets file"),
    }
}

#[cfg(not(unix))]
fn warn_if_world_readable(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_never_debug_prints_its_value() {
        let s = Secret::new("hunter2".into());
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert_eq!(s.expose(), "hunter2");
    }

    #[test]
    fn secret_deserializes_transparently() {
        #[derive(Deserialize)]
        struct T {
            password: Option<Secret>,
        }
        let t: T = serde_yaml::from_str("password: pw\n").unwrap();
        assert_eq!(t.password.unwrap().expose(), "pw");
    }
}
