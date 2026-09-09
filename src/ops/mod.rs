//! The five operations. Each returns the names of items that failed, so the
//! caller can choose the process exit code (empty = clean run).

mod archive;
pub mod backup;
mod cleanup;
pub mod restore;
mod verify;

pub use archive::run as archive;
pub use backup::{run as backup, run_with_store as backup_with_store, BackupReport};
pub use cleanup::{run as cleanup, CleanupAction};
pub use restore::{
    list_backups, run as restore, run_with_store as restore_with_store, RestoreRequest,
};
pub use verify::{run as verify, VerifyRequest};
