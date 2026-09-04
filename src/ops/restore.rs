//! Restore: reconstruct one source from a chosen backup into a target.

use tracing::info;

use crate::cli::RestoreAction;
use crate::config::{check_not_production, resolve_target, Config, ProcessEnv, TargetOverrides};
use crate::engine::ensure_engine;
use crate::error::{ArkError, Result};

/// What the user asked `restore` to do.
#[derive(Debug, Clone)]
pub struct RestoreRequest {
    pub source: String,
    pub action: RestoreAction,
    /// `latest`, a stamp / object key, or a local dump path.
    pub from: String,
    pub target: TargetOverrides,
}

/// Restore a single source. Returns the source name on failure (so the exit
/// code reflects it) — the request is one source, never a loop. Paths that
/// have no backend yet return [`ArkError::NotImplemented`] rather than
/// pretending the item failed.
pub async fn run(config: &Config, request: &RestoreRequest, dry_run: bool) -> Result<Vec<String>> {
    let source = config.source(&request.source)?;

    match request.action {
        // Metadata only: needs the object store, not the engine.
        RestoreAction::ListBackups => {
            // TODO(M0-2): list `<folder>/<source>/versioned/` newest first.
            Err(ArkError::NotImplemented(
                "restore list-backups (object store lands in M0-2)",
            ))
        }
        RestoreAction::Restore => {
            ensure_engine(source.source_type)?;
            let target = resolve_target(config, source, &request.target, &ProcessEnv)?;
            check_not_production(source, &target)?;
            info!(
                source = %source.name,
                target = %target.name,
                from = %request.from,
                dry_run,
                "target resolved and never-production guard passed"
            );
            // TODO(M0-4): empty-target check -> download -> safe-extract ->
            // manifest validation -> load plan -> load -> summary.
            Err(ArkError::NotImplemented("restore backend (M0-4)"))
        }
    }
}
