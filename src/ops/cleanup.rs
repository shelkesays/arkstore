//! Cleanup: apply calendar-tier retention to stored backups.

use clap::ValueEnum;
use tracing::info;

use crate::config::Config;
use crate::error::Result;

/// The cleanup sub-actions (plan/execute/run/consolidate).
#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum CleanupAction {
    /// Scan and emit a plan + report; delete nothing.
    GeneratePlan,
    /// Execute a previously generated (and validated) plan.
    ExecutePlan,
    /// Generate, persist, execute, then consolidate the audit trail.
    Run,
    /// Roll audit files up into one file per period.
    ConsolidatePlans,
}

/// Run the requested cleanup action. Returns failed item names (empty = clean).
pub async fn run(
    config: &Config,
    action: CleanupAction,
    only: Option<&str>,
    dry_run: bool,
) -> Result<Vec<String>> {
    if !config.cleanup.enable {
        info!("cleanup is disabled in config");
        return Ok(vec![]);
    }
    // TODO(M1): scan bucket -> band into tiers -> plan (validated) -> execute
    // -> consolidate. Never delete the latest pointer, today's backups, or an
    // unparsable key.
    info!(?action, source = ?only, dry_run, "cleanup not yet implemented");
    Ok(vec![])
}
