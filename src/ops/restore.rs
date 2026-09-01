//! Restore: reconstruct a database or file tree from a chosen backup.

use tracing::{info, warn};

use crate::config::Config;
use crate::engine::ensure_engine;
use crate::error::Result;

/// Restore every enabled source (or one, via `only`) from object storage.
///
/// Returns the names of sources that failed.
pub fn run(config: &Config, only: Option<&str>, dry_run: bool) -> Result<Vec<String>> {
    let sources = config.selected_sources(only);
    if sources.is_empty() {
        warn!("no enabled sources selected for restore");
        return Ok(vec![]);
    }

    let mut failed = Vec::new();
    for source in sources {
        if let Err(err) = ensure_engine(source.source_type) {
            warn!(source = %source.name, %err, "skipping source");
            failed.push(source.name.clone());
            continue;
        }
        // TODO(M0): resolve backup key -> download -> decompress -> load target.
        info!(source = %source.name, dry_run, "restore not yet implemented");
    }
    Ok(failed)
}
