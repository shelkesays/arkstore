//! Backup: dump databases / snapshot file trees to object storage.
//!
//! File sources are complete here (pack → upload + verify → `latest` pointer →
//! local lifecycle). Database sources need their engine backends (M0-3+).

use std::path::{Path, PathBuf};

use chrono::Utc;
use chrono_tz::Tz;
use tracing::{info, warn};

use crate::config::{Config, Source, SourceType};
use crate::engine::ensure_engine;
use crate::error::{ArkError, Result};
use crate::layout::{
    latest_file_name, latest_key, parse_key, render_stamp, versioned_file_name, versioned_key,
    BackupKind,
};
use crate::pack::{pack_tree, IgnoreRules, PackReport};
use crate::store::Store;

/// What one source's backup produced.
#[derive(Debug, Clone)]
pub struct BackupReport {
    pub source: String,
    pub stamp: String,
    pub size: u64,
    pub sha256: String,
    /// Versioned object key, when uploaded.
    pub key: Option<String>,
    /// Local versioned copy, when kept.
    pub local_path: Option<PathBuf>,
}

/// Back up every enabled source, optionally narrowed by engine type and/or
/// name, against the configured store.
pub async fn run(
    config: &Config,
    kind: Option<SourceType>,
    only: Option<&str>,
    dry_run: bool,
) -> Result<Vec<String>> {
    let needs_store = !dry_run
        && config
            .selected_sources(kind, only)
            .iter()
            .any(|s| s.backup_to_s3);
    let store = if needs_store {
        Some(Store::from_config(config)?)
    } else {
        None
    };
    run_with_store(config, store.as_ref(), kind, only, dry_run).await
}

/// [`run`] against an explicit store (tests use a local one). Per-source
/// failures are isolated; returns the names of sources that failed.
pub async fn run_with_store(
    config: &Config,
    store: Option<&Store>,
    kind: Option<SourceType>,
    only: Option<&str>,
    dry_run: bool,
) -> Result<Vec<String>> {
    let sources = config.selected_sources(kind, only);
    if sources.is_empty() {
        warn!("no enabled sources selected for backup");
        return Ok(vec![]);
    }
    let tz = config.timezone()?;
    let mut failed = Vec::new();
    for source in sources {
        match backup_one(config, store, tz, source, dry_run).await {
            Ok(report) => info!(
                source = %report.source,
                stamp = %report.stamp,
                size = report.size,
                key = report.key.as_deref().unwrap_or("-"),
                local = %report.local_path.as_deref().map(Path::display).map(|d| d.to_string()).unwrap_or_default(),
                dry_run,
                "backup complete"
            ),
            Err(err) => {
                warn!(source = %source.name, %err, "backup failed; continuing with the next source");
                failed.push(source.name.clone());
            }
        }
    }
    Ok(failed)
}

async fn backup_one(
    config: &Config,
    store: Option<&Store>,
    tz: Tz,
    source: &Source,
    dry_run: bool,
) -> Result<BackupReport> {
    ensure_engine(source.source_type)?;
    let stamp = render_stamp(Utc::now(), tz);
    match source.source_type {
        SourceType::File => backup_file_source(config, store, source, &stamp, dry_run).await,
        SourceType::Postgre | SourceType::Mysql | SourceType::Mongo => {
            Err(ArkError::NotImplemented("database backup backend (M0-3)"))
        }
    }
}

async fn backup_file_source(
    config: &Config,
    store: Option<&Store>,
    source: &Source,
    stamp: &str,
    dry_run: bool,
) -> Result<BackupReport> {
    let root = PathBuf::from(source.path.as_deref().unwrap_or_default());
    if !root.is_dir() {
        return Err(ArkError::Validation(format!(
            "source `{}`: path `{}` is not a directory",
            source.name,
            root.display()
        )));
    }
    let rules = IgnoreRules::from_source(source);
    if dry_run {
        return dry_run_file_source(source, &root, &rules, stamp);
    }

    let work = tempfile::tempdir()?;
    let archive = work.path().join(versioned_file_name(&source.name, stamp));
    let packed = pack_in_background(root, rules, archive).await?;
    let key = if source.backup_to_s3 {
        Some(upload_versioned(store, config, source, stamp, &packed).await?)
    } else {
        None
    };
    let local_path = keep_local_if_needed(config, source, stamp, &packed)?;
    Ok(BackupReport {
        source: source.name.clone(),
        stamp: stamp.to_string(),
        size: packed.size,
        sha256: packed.sha256,
        key,
        local_path,
    })
}

async fn pack_in_background(
    root: PathBuf,
    rules: IgnoreRules,
    archive: PathBuf,
) -> Result<PackReport> {
    tokio::task::spawn_blocking(move || pack_tree(&root, &rules, &archive))
        .await
        .map_err(|e| ArkError::Internal(format!("packing task failed: {e}")))?
}

/// A local copy is kept when the source is local-only or asked to keep
/// copies after upload.
fn keep_local_if_needed(
    config: &Config,
    source: &Source,
    stamp: &str,
    packed: &PackReport,
) -> Result<Option<PathBuf>> {
    if source.backup_to_s3 && source.delete_after_upload {
        return Ok(None);
    }
    keep_local_copy(&config.app.local_dir, source, stamp, &packed.path).map(Some)
}

fn dry_run_file_source(
    source: &Source,
    root: &Path,
    rules: &IgnoreRules,
    stamp: &str,
) -> Result<BackupReport> {
    let (mut kept, mut excluded) = (0u64, 0u64);
    for entry in std::fs::read_dir(root)? {
        let name = entry?.file_name();
        if rules.excludes(&name.to_string_lossy()) {
            excluded = excluded.saturating_add(1);
        } else {
            kept = kept.saturating_add(1);
        }
    }
    info!(
        source = %source.name,
        path = %root.display(),
        top_level_kept = kept,
        top_level_excluded = excluded,
        would_upload = source.backup_to_s3,
        "dry run: would pack and upload"
    );
    Ok(BackupReport {
        source: source.name.clone(),
        stamp: stamp.to_string(),
        size: 0,
        sha256: String::new(),
        key: None,
        local_path: None,
    })
}

/// Upload the versioned object, verify it, then rewrite the `latest` pointer.
async fn upload_versioned(
    store: Option<&Store>,
    config: &Config,
    source: &Source,
    stamp: &str,
    packed: &PackReport,
) -> Result<String> {
    let store =
        store.ok_or_else(|| ArkError::Store("no object store configured for upload".into()))?;
    let folder = &config.aws.folder;
    let key = versioned_key(folder, &source.name, stamp);
    let report = store.upload_file(&key, &packed.path).await?;
    if report.sha256 != packed.sha256 {
        return Err(ArkError::Store(format!(
            "upload of `{key}` sent bytes that differ from the packed archive (sha256 mismatch)"
        )));
    }
    store.copy(&key, &latest_key(folder, &source.name)).await?;
    Ok(key)
}

/// Copy the finished archive into `<local_dir>/<source>/versioned/`, refresh
/// the local `latest` copy, then prune to `local_retention`.
fn keep_local_copy(
    local_dir: &Path,
    source: &Source,
    stamp: &str,
    archive: &Path,
) -> Result<PathBuf> {
    let source_dir = local_dir.join(&source.name);
    let versioned_dir = source_dir.join("versioned");
    std::fs::create_dir_all(&versioned_dir)?;
    let dest = versioned_dir.join(versioned_file_name(&source.name, stamp));
    std::fs::copy(archive, &dest)?;
    std::fs::copy(&dest, source_dir.join(latest_file_name(&source.name)))?;
    prune_local(&versioned_dir, source)?;
    Ok(dest)
}

/// Keep the newest `local_retention` versioned archives (0 = keep all).
fn prune_local(versioned_dir: &Path, source: &Source) -> Result<()> {
    let keep = usize::try_from(source.local_retention).unwrap_or(usize::MAX);
    if keep == 0 {
        return Ok(());
    }
    let mut stamped: Vec<(String, PathBuf)> = std::fs::read_dir(versioned_dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let key = format!("{}/versioned/{name}", source.name);
            match parse_key("", &key).map(|p| p.kind) {
                Some(BackupKind::Versioned { stamp }) => Some((stamp, e.path())),
                _ => None,
            }
        })
        .collect();
    stamped.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in stamped.into_iter().skip(keep) {
        info!(path = %path.display(), "pruning local archive beyond local_retention");
        std::fs::remove_file(path)?;
    }
    Ok(())
}
