//! Database engines, opt-in at compile time via Cargo features.
//!
//! Each engine (`postgres`, `mysql`, `mongo`, `files`) is a feature. Requesting
//! an engine that was not compiled in fails fast with a clear rebuild message
//! instead of a cryptic error — the compile-time analogue of install extras.

#[cfg(feature = "postgres")]
pub mod postgres;

use std::path::Path;

use crate::config::{Source, SourceType};
use crate::error::Result;
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
