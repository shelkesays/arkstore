//! The five operations. Each returns the names of items that failed, so the
//! caller can choose the process exit code (empty = clean run).

mod archive;
mod backup;
mod cleanup;
mod restore;
mod verify;

pub use archive::run as archive;
pub use backup::run as backup;
pub use cleanup::{run as cleanup, CleanupAction};
pub use restore::{run as restore, RestoreRequest};
pub use verify::{run as verify, VerifyRequest};
