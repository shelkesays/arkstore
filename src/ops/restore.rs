//! Restore: reconstruct one source from a chosen backup into a target.
//!
//! `list-backups` and file-tree restore are complete here; database restore
//! needs the engine loaders (M0-4).

use std::path::{Path, PathBuf};

use tracing::{info, warn};

use crate::cli::RestoreAction;
use crate::config::{
    check_not_production, resolve_target, Config, ProcessEnv, ResolvedTarget, Source, SourceType,
    TargetOverrides,
};
use crate::engine::ensure_engine;
use crate::error::{ArkError, Result};
use crate::layout::{
    latest_key, parse_key, parse_stamp, source_prefix, versioned_key, versioned_prefix, BackupKind,
};
use crate::pack::{ensure_headroom, unpack};
use crate::store::{ObjectInfo, Store};

/// What the user asked `restore` to do.
#[derive(Debug, Clone)]
pub struct RestoreRequest {
    pub source: String,
    pub action: RestoreAction,
    /// `latest`, a stamp, a full object key under the source's prefix, or a
    /// local archive path.
    pub from: String,
    pub target: TargetOverrides,
}

/// Restore a single source against the configured store.
pub async fn run(config: &Config, request: &RestoreRequest, dry_run: bool) -> Result<Vec<String>> {
    let store = Store::from_config(config)?;
    run_with_store(config, &store, request, dry_run).await
}

/// [`run`] against an explicit store. Returns the source name on failure —
/// the request is one source, never a loop.
pub async fn run_with_store(
    config: &Config,
    store: &Store,
    request: &RestoreRequest,
    dry_run: bool,
) -> Result<Vec<String>> {
    let source = config.source(&request.source)?;
    match request.action {
        RestoreAction::ListBackups => list_backups(config, store, source).await.map(|_| vec![]),
        RestoreAction::Restore => {
            ensure_engine(source.source_type)?;
            let target = resolve_target(config, source, &request.target, &ProcessEnv)?;
            check_not_production(source, &target)?;
            match source.source_type {
                SourceType::File => {
                    restore_file_source(config, store, source, &target, &request.from, dry_run)
                        .await
                }
                _ => Err(ArkError::NotImplemented("database restore backend (M0-4)")),
            }
            .map(|_| vec![])
        }
    }
}

/// List the versioned backups for `source`, newest first.
pub async fn list_backups(
    config: &Config,
    store: &Store,
    source: &Source,
) -> Result<Vec<ObjectInfo>> {
    let folder = &config.aws.folder;
    let mut objects: Vec<(String, ObjectInfo)> = store
        .list(&versioned_prefix(folder, &source.name))
        .await?
        .into_iter()
        .filter_map(|o| match parse_key(folder, &o.key).map(|p| p.kind) {
            Some(BackupKind::Versioned { stamp }) => Some((stamp, o)),
            _ => None,
        })
        .collect();
    objects.sort_by(|a, b| b.0.cmp(&a.0));
    info!(source = %source.name, count = objects.len(), store = %store.label(), "versioned backups (newest first)");
    for (stamp, object) in &objects {
        info!(stamp, key = %object.key, size = object.size, last_modified = %object.last_modified.to_rfc3339(), "backup");
    }
    Ok(objects.into_iter().map(|(_, o)| o).collect())
}

/// Where a backup comes from.
enum BackupSelection {
    Object(String),
    LocalFile(PathBuf),
}

/// Resolve `--from`: `latest`, a stamp, a local archive path, or a full key
/// that must stay under the source's own prefix (PRD §9.6 confinement).
fn select_backup(config: &Config, source: &Source, from: &str) -> Result<BackupSelection> {
    let folder = &config.aws.folder;
    if from == "latest" {
        return Ok(BackupSelection::Object(latest_key(folder, &source.name)));
    }
    if parse_stamp(from).is_some() {
        return Ok(BackupSelection::Object(versioned_key(
            folder,
            &source.name,
            from,
        )));
    }
    let local = Path::new(from);
    if local.is_file() {
        return Ok(BackupSelection::LocalFile(local.to_path_buf()));
    }
    let prefix = source_prefix(folder, &source.name);
    if from.starts_with(&prefix) && !from.contains("..") && parse_key(folder, from).is_some() {
        return Ok(BackupSelection::Object(from.to_string()));
    }
    Err(ArkError::Refused(format!(
        "`--from {from}` is not `latest`, a stamp, an existing local file, or a backup key under `{prefix}`"
    )))
}

async fn restore_file_source(
    config: &Config,
    store: &Store,
    source: &Source,
    target: &ResolvedTarget,
    from: &str,
    dry_run: bool,
) -> Result<()> {
    let dest = PathBuf::from(target.path.as_deref().unwrap_or_default());
    ensure_empty_dir(&dest)?;
    let selection = select_backup(config, source, from)?;

    let work = tempfile::tempdir()?;
    let Some((archive, size)) =
        fetch_selection(store, source, &selection, work.path(), &dest, dry_run).await?
    else {
        return Ok(());
    };
    extract_archive(source, target, &dest, archive, size).await
}

/// Materialise the selected backup as a local archive file; `None` on a dry
/// run (after reporting what would happen).
async fn fetch_selection(
    store: &Store,
    source: &Source,
    selection: &BackupSelection,
    work: &Path,
    dest: &Path,
    dry_run: bool,
) -> Result<Option<(PathBuf, u64)>> {
    match selection {
        BackupSelection::Object(key) => {
            if dry_run {
                let meta = store.head(key).await?;
                info!(source = %source.name, key, size = meta.size, target = %dest.display(), "dry run: would download and extract");
                return Ok(None);
            }
            let path = work.join("backup.tar.gz");
            let size = store.download_to_file(key, &path).await?;
            Ok(Some((path, size)))
        }
        BackupSelection::LocalFile(path) => {
            let size = std::fs::metadata(path)?.len();
            if dry_run {
                info!(source = %source.name, archive = %path.display(), size, target = %dest.display(), "dry run: would extract");
                return Ok(None);
            }
            Ok(Some((path.clone(), size)))
        }
    }
}

async fn extract_archive(
    source: &Source,
    target: &ResolvedTarget,
    dest: &Path,
    archive: PathBuf,
    size: u64,
) -> Result<()> {
    let headroom_dir = dest
        .parent()
        .filter(|p| p.exists())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    ensure_headroom(&headroom_dir, size)?;
    let dest_owned = dest.to_path_buf();
    let report = tokio::task::spawn_blocking(move || unpack(&archive, &dest_owned))
        .await
        .map_err(|e| ArkError::Internal(format!("extraction task failed: {e}")))??;
    if !report.skipped.is_empty() {
        warn!(source = %source.name, skipped = report.skipped.len(), "some archive entries were refused (see warnings above)");
    }
    info!(source = %source.name, target = %target.name, entries = report.entries, bytes = report.bytes, "file restore complete");
    Ok(())
}

/// The target directory must be absent or empty (PRD §6.2 step 3).
fn ensure_empty_dir(dest: &Path) -> Result<()> {
    if dest.is_file() {
        return Err(ArkError::Refused(format!(
            "target `{}` is a file, not a directory",
            dest.display()
        )));
    }
    if dest.is_dir() && std::fs::read_dir(dest)?.next().is_some() {
        return Err(ArkError::Refused(format!(
            "target directory `{}` is not empty — restore only writes into an empty target",
            dest.display()
        )));
    }
    Ok(())
}
