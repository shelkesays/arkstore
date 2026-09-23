//! Restore: reconstruct one source from a chosen backup into a target.
//!
//! `list-backups`, file-tree restore, and database restore through the native
//! engine loaders (KB §5): target guard, strict empty-target check before any
//! transfer, safe extraction, manifest validation, load-plan order, per-object
//! failure isolation, and a `{restored, skipped, failed}` summary.

use std::path::{Path, PathBuf};

use tracing::{info, warn};

use crate::cli::RestoreAction;
use crate::config::{
    check_not_production, resolve_target, Config, ProcessEnv, ResolvedTarget, Source, SourceType,
    TargetOverrides,
};
use crate::engine::{
    ensure_engine, restore_database, target_contents, target_defines, RestoreOutcome,
};
use crate::error::{ArkError, Result};
use crate::layout::{
    latest_key, parse_key, parse_stamp, source_prefix, versioned_key, versioned_prefix, BackupKind,
};
use crate::manifest::{Manifest, ObjectEntry};
use crate::pack::{ensure_headroom, unpack, UnpackReport};
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
                        .map(|()| vec![])
                }
                _ => {
                    restore_database_source(config, store, source, &target, &request.from, dry_run)
                        .await
                }
            }
        }
    }
}

/// Database restore (KB §5.4). Returns the source name when any object
/// failed, so the run exits `1` while every other object still landed.
async fn restore_database_source(
    config: &Config,
    store: &Store,
    source: &Source,
    target: &ResolvedTarget,
    from: &str,
    dry_run: bool,
) -> Result<Vec<String>> {
    let selection = select_backup(config, source, from)?;
    let work = tempfile::tempdir()?;
    let extract = work.path().join("extract");
    let staged = stage_local(source, &selection, &extract).await?;
    guard_target(target, staged.as_ref()).await?;
    if dry_run {
        return report_database_dry_run(store, source, target, &selection, staged.as_ref()).await;
    }
    let manifest = match staged {
        Some(manifest) => manifest,
        None => fetch_and_unpack(store, source, &selection, work.path(), &extract).await?,
    };
    let outcome = restore_database(target, &extract, &manifest).await?;
    Ok(summarise(source, target, &outcome))
}

/// Log the `{restored, skipped, failed}` summary; the source name comes back
/// when anything failed so the run exits `1`.
fn summarise(source: &Source, target: &ResolvedTarget, outcome: &RestoreOutcome) -> Vec<String> {
    for (object, reason) in &outcome.failed {
        warn!(source = %source.name, target = %target.name, object, reason, "object failed");
    }
    info!(
        source = %source.name,
        target = %target.name,
        restored = outcome.restored.len(),
        skipped = outcome.skipped.len(),
        failed = outcome.failed.len(),
        "database restore finished"
    );
    if outcome.failed.is_empty() {
        vec![]
    } else {
        vec![source.name.clone()]
    }
}

/// A local archive is unpacked first so a single-item archive can be
/// recognised; a stored backup is only fetched once the target is proven
/// empty (PRD §6.2 step 3).
async fn stage_local(
    source: &Source,
    selection: &BackupSelection,
    extract: &Path,
) -> Result<Option<Manifest>> {
    match selection {
        BackupSelection::LocalFile(path) => {
            unpack_into(source, path, extract).await?;
            read_manifest(source, extract).map(Some)
        }
        BackupSelection::Object(_) => Ok(None),
    }
}

/// Single-item archives need only their object absent; everything else
/// needs an empty target.
async fn guard_target(target: &ResolvedTarget, staged: Option<&Manifest>) -> Result<()> {
    match staged.and_then(Manifest::single_item) {
        Some(object) => ensure_object_absent(target, object).await,
        None => ensure_target_empty(target).await,
    }
}

async fn fetch_and_unpack(
    store: &Store,
    source: &Source,
    selection: &BackupSelection,
    work: &Path,
    extract: &Path,
) -> Result<Manifest> {
    let (archive, _) = fetch_selection(store, source, selection, work, extract, false)
        .await?
        .ok_or_else(|| ArkError::Internal("fetch returned nothing outside a dry run".into()))?;
    unpack_into(source, &archive, extract).await?;
    read_manifest(source, extract)
}

/// Safe-extract `archive` into `dest`; any refused entry fails the restore.
async fn unpack_into(source: &Source, archive: &Path, dest: &Path) -> Result<()> {
    let size = std::fs::metadata(archive)?.len();
    let parent = dest.parent().map(Path::to_path_buf).unwrap_or_default();
    ensure_headroom(&parent, size)?;
    let (archive, dest_owned) = (archive.to_path_buf(), dest.to_path_buf());
    let report = tokio::task::spawn_blocking(move || unpack(&archive, &dest_owned))
        .await
        .map_err(|e| ArkError::Internal(format!("extraction task failed: {e}")))??;
    if !report.skipped.is_empty() {
        return Err(refused_entries(source, dest, &report));
    }
    Ok(())
}

/// Read and validate `manifest.json`, check it belongs to this source, and
/// warn about files the manifest does not list (never loaded, KB §2.5).
fn read_manifest(source: &Source, dest: &Path) -> Result<Manifest> {
    let manifest = Manifest::from_json(&std::fs::read(dest.join("manifest.json"))?)?;
    if manifest.source != source.name {
        return Err(ArkError::Refused(format!(
            "archive was taken from source `{}`, not `{}`",
            manifest.source, source.name
        )));
    }
    if manifest.engine != source.source_type {
        return Err(ArkError::Refused(format!(
            "archive was taken from a {} source, but `{}` is {}",
            manifest.engine.display_name(),
            source.name,
            source.source_type.display_name()
        )));
    }
    let listed: std::collections::HashSet<&str> = manifest.file_paths().collect();
    for entry in std::fs::read_dir(dest)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name != "manifest.json" && !listed.contains(name.as_str()) {
            warn!(source = %source.name, file = %name, "archive file not listed in the manifest; never loaded");
        }
    }
    Ok(manifest)
}

/// Strict, engine-defined "empty" before any transfer (PRD §6.2 step 3).
async fn ensure_target_empty(target: &ResolvedTarget) -> Result<()> {
    let contents = target_contents(target).await?;
    if contents.total == 0 {
        return Ok(());
    }
    Err(ArkError::Refused(format!(
        "target `{}` ({}) is not empty: {} user objects, e.g. {} — restore only writes into an empty target",
        target.name,
        target.database.as_deref().unwrap_or("?"),
        contents.total,
        contents.sample.join(", ")
    )))
}

/// Single-item rule: the one object must be absent from the target.
/// Single-item rule: the one object must be absent from the target. A kind
/// the engine cannot look up falls back to the whole-target empty check.
async fn ensure_object_absent(target: &ResolvedTarget, object: &ObjectEntry) -> Result<()> {
    match target_defines(target, object).await? {
        Some(true) => Err(ArkError::Refused(format!(
            "target `{}` already defines `{}` — a single-item restore needs it absent",
            target.name, object.name
        ))),
        Some(false) => Ok(()),
        None => ensure_target_empty(target).await,
    }
}

async fn report_database_dry_run(
    store: &Store,
    source: &Source,
    target: &ResolvedTarget,
    selection: &BackupSelection,
    staged: Option<&Manifest>,
) -> Result<Vec<String>> {
    match selection {
        BackupSelection::Object(key) => {
            let meta = store.head(key).await?;
            info!(source = %source.name, target = %target.name, key, size = meta.size, "dry run: target is empty; would download, extract and load");
        }
        BackupSelection::LocalFile(path) => {
            let objects = staged.map(|m| m.objects.len()).unwrap_or_default();
            let layers = staged
                .map(|m| m.load_plan().layers.len())
                .unwrap_or_default();
            info!(source = %source.name, target = %target.name, archive = %path.display(), objects, layers, "dry run: would load");
        }
    }
    Ok(vec![])
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

/// Resolve `--from`, in this fixed precedence: `latest`, a stamp, a full key
/// under the source's own prefix (PRD §9.6 confinement), then a local archive
/// path. Object forms win, so a local file that happens to share a key's
/// spelling can never shadow the stored backup.
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
    let prefix = source_prefix(folder, &source.name);
    if from.starts_with(&prefix) && !from.contains("..") && parse_key(folder, from).is_some() {
        return Ok(BackupSelection::Object(from.to_string()));
    }
    let local = Path::new(from);
    if local.is_file() {
        return Ok(BackupSelection::LocalFile(local.to_path_buf()));
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
        return Err(refused_entries(source, dest, &report));
    }
    info!(source = %source.name, target = %target.name, entries = report.entries, bytes = report.bytes, "file restore complete");
    Ok(())
}

/// A refused entry means the archive is not one Arkstore produced or the
/// target changed underneath us; either way the restore is incomplete and
/// must not be reported as success. The target is left for inspection.
fn refused_entries(source: &Source, dest: &Path, report: &UnpackReport) -> ArkError {
    let first: Vec<&str> = report.skipped.iter().take(3).map(String::as_str).collect();
    ArkError::Refused(format!(
        "restore of `{}` is incomplete: {} archive entries were refused (e.g. {}); target `{}` is left as-is for inspection",
        source.name,
        report.skipped.len(),
        first.join("; "),
        dest.display()
    ))
}

/// The target must be absent, or a real (not symlinked) empty directory
/// (PRD §6.2 step 3). A symlink is refused even when it points at an empty
/// directory: extraction would otherwise write outside the requested path.
fn ensure_empty_dir(dest: &Path) -> Result<()> {
    let meta = match std::fs::symlink_metadata(dest) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    if meta.file_type().is_symlink() {
        return Err(ArkError::Refused(format!(
            "target `{}` is a symlink — restore only writes into a real directory",
            dest.display()
        )));
    }
    if !meta.is_dir() {
        return Err(ArkError::Refused(format!(
            "target `{}` is a file, not a directory",
            dest.display()
        )));
    }
    if std::fs::read_dir(dest)?.next().is_some() {
        return Err(ArkError::Refused(format!(
            "target directory `{}` is not empty — restore only writes into an empty target",
            dest.display()
        )));
    }
    Ok(())
}
