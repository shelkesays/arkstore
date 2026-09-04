//! Archive: move aged rows from a live DB into Parquet in object storage.

use tracing::{info, warn};

use crate::config::Config;
use crate::engine::ensure_engine;
use crate::error::Result;

/// Archive every enabled, archivable source that declares archive rules.
///
/// Sources with no rules are logged and skipped. Returns failed source names.
pub async fn run(config: &Config, only: Option<&str>, dry_run: bool) -> Result<Vec<String>> {
    if !config.archive.enable {
        warn!("archive is disabled in config");
        return Ok(vec![]);
    }
    let dry_run = dry_run || config.archive.dry_run;

    let mut failed = Vec::new();
    for source in config.selected_sources(None, only) {
        if !source.source_type.is_archivable() {
            continue;
        }
        if source.archive.is_empty() {
            info!(source = %source.name, "no archive rules configured; skipping");
            continue;
        }
        if let Err(err) = ensure_engine(source.source_type) {
            warn!(source = %source.name, %err, "skipping source");
            failed.push(source.name.clone());
            continue;
        }
        // TODO(M2): per rule, per whole month older than the cutoff:
        // count -> fetch -> parquet -> upload -> verify -> delete.
        info!(
            source = %source.name,
            rules = source.archive.len(),
            dry_run,
            "archive not yet implemented"
        );
    }
    Ok(failed)
}
