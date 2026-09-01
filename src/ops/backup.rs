//! Backup: dump databases / snapshot file trees to object storage.

use tracing::{info, warn};

use crate::config::Config;
use crate::engine::ensure_engine;
use crate::error::Result;

/// Back up every enabled source (or one, via `only`).
///
/// Returns the names of sources that failed; per-source failures are isolated so
/// one bad source never aborts the run.
pub fn run(config: &Config, only: Option<&str>, dry_run: bool) -> Result<Vec<String>> {
    let sources = config.selected_sources(only);
    if sources.is_empty() {
        warn!("no enabled sources selected for backup");
        return Ok(vec![]);
    }

    let mut failed = Vec::new();
    for source in sources {
        if let Err(err) = ensure_engine(source.source_type) {
            warn!(source = %source.name, %err, "skipping source");
            failed.push(source.name.clone());
            continue;
        }
        // TODO(M0): dump -> compress -> upload -> verify -> update `latest`.
        info!(source = %source.name, dry_run, "backup not yet implemented");
    }
    Ok(failed)
}
