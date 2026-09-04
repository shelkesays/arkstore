//! Backup: dump databases / snapshot file trees to object storage.

use tracing::{info, warn};

use crate::config::{Config, SourceType};
use crate::engine::ensure_engine;
use crate::error::Result;

/// Back up every enabled source, optionally narrowed by engine type and/or
/// name. Returns the names of sources that failed; per-source failures are
/// isolated so one bad source never aborts the run.
pub async fn run(
    config: &Config,
    kind: Option<SourceType>,
    only: Option<&str>,
    dry_run: bool,
) -> Result<Vec<String>> {
    let sources = config.selected_sources(kind, only);
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
        // TODO(M0-3): snapshot -> enumerate -> dump -> manifest -> package
        // -> upload -> verify -> latest pointer -> local lifecycle.
        info!(
            source = %source.name,
            structure = source.effective_structure(),
            data = source.data,
            dry_run,
            "backup backend not yet implemented"
        );
    }
    Ok(failed)
}
