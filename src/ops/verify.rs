//! Verify: prove a backup is restorable by round-tripping it into a throwaway
//! target and diffing it against the manifest baseline (PRD §6.5).

use tracing::{info, warn};

use crate::config::{Config, TargetOverrides};
use crate::engine::ensure_engine;
use crate::error::Result;

/// What the user asked `verify` to do.
#[derive(Debug, Clone)]
pub struct VerifyRequest {
    pub source: String,
    pub from: String,
    pub target: TargetOverrides,
}

/// Verify one source's backup. Returns the source name on failure.
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
    warn!(source = %source.name, "verify not yet implemented (M0-5)");
    Ok(vec![source.name.clone()])
}
