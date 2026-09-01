//! Database engines, opt-in at compile time via Cargo features.
//!
//! Each engine (`postgres`, `mysql`, `mongo`, `files`) is a feature. Requesting
//! an engine that was not compiled in fails fast with a clear rebuild message
//! instead of a cryptic error — the compile-time analogue of install extras.

use crate::config::SourceType;
use crate::error::Result;

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
