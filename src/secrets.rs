//! Secrets loading. Credentials never live in the tracked config; they are
//! merged in at load time from a secrets manager or a local secrets file.

use crate::config::Config;
use crate::error::Result;

/// A source of credentials that hydrates a [`Config`] in place, keyed by source
/// name.
pub trait SecretsProvider {
    fn hydrate(&self, config: &mut Config) -> Result<()>;
}

/// Assumes credentials are already present in the config (dev / self-hosted).
/// Real providers (local file, AWS Secrets Manager) land in M0.
pub struct InlineSecrets;

impl SecretsProvider for InlineSecrets {
    fn hydrate(&self, _config: &mut Config) -> Result<()> {
        Ok(())
    }
}
