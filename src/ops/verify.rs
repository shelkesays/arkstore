//! Verify: prove a backup is restorable by round-tripping it into a throwaway
//! target and diffing it against the manifest baseline (PRD §6.5).

use tracing::info;

use crate::config::{Config, TargetOverrides};
use crate::engine::ensure_engine;
use crate::error::{ArkError, Result};

/// What the user asked `verify` to do.
#[derive(Debug, Clone)]
pub struct VerifyRequest {
    pub source: String,
    pub from: String,
    pub target: TargetOverrides,
}

/// Verify one source's backup. Until the backend lands this returns
/// [`ArkError::NotImplemented`] — never a fake success or a fake item failure.
pub async fn run(config: &Config, request: &VerifyRequest, dry_run: bool) -> Result<Vec<String>> {
    let source = config.source(&request.source)?;
    ensure_engine(source.source_type)?;
    info!(
        source = %source.name,
        from = %request.from,
        has_verify_server = config.verify.server.is_some(),
        dry_run,
        "verify request accepted"
    );
    // TODO(M0-5): ephemeral target -> restore -> re-introspect -> compare
    // schema_hash + counts + content_hash -> report -> tear down.
    Err(ArkError::NotImplemented("verify (M0-5)"))
}
