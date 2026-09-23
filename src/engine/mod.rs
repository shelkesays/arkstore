//! Database engines, opt-in at compile time via Cargo features.
//!
//! Each engine (`postgres`, `mysql`, `mongo`, `files`) is a feature. Requesting
//! an engine that was not compiled in fails fast with a clear rebuild message
//! instead of a cryptic error — the compile-time analogue of install extras.

#[cfg(feature = "postgres")]
pub mod postgres;

use std::path::Path;

use crate::config::{ResolvedTarget, Source, SourceType};
use crate::error::{ArkError, Result};
use crate::manifest::Manifest;

/// What a database dump needs to know besides the source itself.
#[derive(Debug, Clone)]
pub struct DumpContext<'a> {
    /// Directory the dump files and `manifest.json` are written into.
    pub work_dir: &'a Path,
    /// The backup stamp the archive will be keyed by.
    pub stamp: &'a str,
    /// The `app.timezone` name the stamp was rendered in.
    pub timezone: &'a str,
}

/// Counts a dry run reports instead of dumping.
#[derive(Debug, Clone, Default)]
pub struct DumpPreview {
    pub server_version: String,
    pub objects: usize,
    pub tables_with_data: usize,
    pub data_skipped: usize,
}

/// Dump `source` through its native backend into `ctx.work_dir`, returning
/// the manifest that was also written there as `manifest.json`.
pub async fn dump_database(source: &Source, ctx: &DumpContext<'_>) -> Result<Manifest> {
    ensure_engine(source.source_type)?;
    match source.source_type {
        #[cfg(feature = "postgres")]
        SourceType::Postgre => postgres::dump(source, ctx).await,
        _ => {
            tracing::debug!(
                source = %source.name,
                work_dir = %ctx.work_dir.display(),
                "no native dump backend for this engine in this build"
            );
            Err(crate::error::ArkError::NotImplemented(
                "database backup backend for this engine (M3)",
            ))
        }
    }
}

/// What a database restore did, per object (PRD §6.2 step 8).
#[derive(Debug, Clone, Default)]
pub struct RestoreOutcome {
    pub restored: Vec<String>,
    /// Listed in the manifest but with nothing to apply.
    pub skipped: Vec<String>,
    /// `(object, reason)` — recorded and skipped, never aborting the run.
    pub failed: Vec<(String, String)>,
}

/// What a target database currently holds, for the empty-target check.
#[derive(Debug, Clone, Default)]
pub struct TargetContents {
    pub total: i64,
    /// Up to a handful of names, for the refusal message.
    pub sample: Vec<String>,
}

/// User objects present in `target` (engine-defined "empty", KB §5.4).
pub async fn target_contents(target: &ResolvedTarget) -> Result<TargetContents> {
    ensure_engine(target.kind)?;
    match target.kind {
        #[cfg(feature = "postgres")]
        SourceType::Postgre => postgres::target_contents(target).await,
        _ => Err(not_implemented_restore(target)),
    }
}

/// Whether `target` already defines `object`; `None` when the engine cannot
/// look that kind up (callers then fall back to the empty-target check).
pub async fn target_defines(
    target: &ResolvedTarget,
    object: &crate::manifest::ObjectEntry,
) -> Result<Option<bool>> {
    ensure_engine(target.kind)?;
    match target.kind {
        #[cfg(feature = "postgres")]
        SourceType::Postgre => postgres::target_defines(target, object).await,
        _ => {
            tracing::debug!(
                object = %object.name,
                "no native loader for this engine in this build"
            );
            Err(not_implemented_restore(target))
        }
    }
}

/// Load an unpacked archive (`dir`, described by `manifest`) into `target`.
pub async fn restore_database(
    target: &ResolvedTarget,
    dir: &Path,
    manifest: &Manifest,
) -> Result<RestoreOutcome> {
    ensure_engine(target.kind)?;
    match target.kind {
        #[cfg(feature = "postgres")]
        SourceType::Postgre => postgres::restore(target, dir, manifest).await,
        _ => {
            tracing::debug!(dir = %dir.display(), objects = manifest.objects.len(), "no native loader for this engine in this build");
            Err(not_implemented_restore(target))
        }
    }
}

/// What `verify` found, per manifest object (PRD §6.5).
#[derive(Debug, Clone, Default)]
pub struct VerifyOutcome {
    pub verified: Vec<String>,
    /// `(object, reason)` — exists but differs, missing, or not in the manifest.
    pub mismatched: Vec<(String, String)>,
    /// `(object, reason)` — the comparison itself could not be made.
    pub failed: Vec<(String, String)>,
}

impl VerifyOutcome {
    pub fn is_clean(&self) -> bool {
        self.mismatched.is_empty() && self.failed.is_empty()
    }
}

/// Re-introspect `target` (restored from `manifest`) and compare it with the
/// manifest baseline on both axes: definitions and data.
pub async fn verify_database(
    source: &Source,
    target: &ResolvedTarget,
    manifest: &Manifest,
) -> Result<VerifyOutcome> {
    ensure_engine(target.kind)?;
    match target.kind {
        #[cfg(feature = "postgres")]
        SourceType::Postgre => postgres::verify(source, target, manifest).await,
        _ => {
            tracing::debug!(source = %source.name, objects = manifest.objects.len(), "no native verifier for this engine in this build");
            Err(not_implemented_restore(target))
        }
    }
}

/// Create the throwaway database `name` on `server` (KB §12).
pub async fn create_database(server: &ResolvedTarget, name: &str) -> Result<()> {
    ensure_engine(server.kind)?;
    match server.kind {
        #[cfg(feature = "postgres")]
        SourceType::Postgre => postgres::create_database(server, name).await,
        _ => {
            tracing::debug!(
                database = name,
                "no native verifier for this engine in this build"
            );
            Err(not_implemented_restore(server))
        }
    }
}

/// Drop a database Arkstore created. Never called for one it did not.
pub async fn drop_database(server: &ResolvedTarget, name: &str) -> Result<()> {
    ensure_engine(server.kind)?;
    match server.kind {
        #[cfg(feature = "postgres")]
        SourceType::Postgre => postgres::drop_database(server, name).await,
        _ => {
            tracing::debug!(
                database = name,
                "no native verifier for this engine in this build"
            );
            Err(not_implemented_restore(server))
        }
    }
}

fn not_implemented_restore(target: &ResolvedTarget) -> ArkError {
    tracing::debug!(target = %target.name, kind = ?target.kind, "restore requested");
    ArkError::NotImplemented("database restore backend for this engine (M3)")
}

/// Connect, open the snapshot, and enumerate — without writing anything.
pub async fn preview_database(source: &Source) -> Result<DumpPreview> {
    ensure_engine(source.source_type)?;
    match source.source_type {
        #[cfg(feature = "postgres")]
        SourceType::Postgre => postgres::preview(source).await,
        _ => Err(crate::error::ArkError::NotImplemented(
            "database backup backend for this engine (M3)",
        )),
    }
}

/// Verify the engine for `source_type` was built into this binary.
pub fn ensure_engine(source_type: SourceType) -> Result<()> {
    match source_type {
        SourceType::Postgre => {
            #[cfg(feature = "postgres")]
            {
                Ok(())
            }
            #[cfg(not(feature = "postgres"))]
            {
                Err(crate::error::ArkError::EngineNotBuilt {
                    engine: "PostgreSQL",
                    feature: "postgres",
                })
            }
        }
        SourceType::Mysql => {
            #[cfg(feature = "mysql")]
            {
                Ok(())
            }
            #[cfg(not(feature = "mysql"))]
            {
                Err(crate::error::ArkError::EngineNotBuilt {
                    engine: "MySQL",
                    feature: "mysql",
                })
            }
        }
        SourceType::Mongo => {
            #[cfg(feature = "mongo")]
            {
                Ok(())
            }
            #[cfg(not(feature = "mongo"))]
            {
                Err(crate::error::ArkError::EngineNotBuilt {
                    engine: "MongoDB",
                    feature: "mongo",
                })
            }
        }
        SourceType::File => {
            #[cfg(feature = "files")]
            {
                Ok(())
            }
            #[cfg(not(feature = "files"))]
            {
                Err(crate::error::ArkError::EngineNotBuilt {
                    engine: "File",
                    feature: "files",
                })
            }
        }
    }
}
