//! Restore: reconstruct one source from a chosen backup into a target.

use tracing::{info, warn};

use crate::cli::RestoreAction;
use crate::config::{check_not_production, resolve_target, Config, ProcessEnv, TargetOverrides};
use crate::engine::ensure_engine;
use crate::error::Result;

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
/// code reflects it) — the request is one source, never a loop.
pub async fn run(config: &Config, request: &RestoreRequest, dry_run: bool) -> Result<Vec<String>> {
    let source = config.source(&request.source)?;
    ensure_engine(source.source_type)?;

    match request.action {
        RestoreAction::ListBackups => {
            // TODO(M0-2): list `<folder>/<source>/versioned/` newest first.
            warn!(source = %source.name, "list-backups needs the object store (M0-2)");
            Ok(vec![source.name.clone()])
        }
        RestoreAction::Restore => {
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
            warn!(source = %source.name, "restore backend not yet implemented (M0-4)");
            Ok(vec![source.name.clone()])
        }
    }
}
