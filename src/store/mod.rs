//! Object-store abstraction. S3-compatible first; GCS/Azure later, behind this
//! same trait.

use crate::error::Result;

/// Metadata for one stored object, as seen during a listing.
#[derive(Debug, Clone)]
pub struct ObjectMeta {
    pub key: String,
    pub size: u64,
    /// Storage class (e.g. `STANDARD`, `GLACIER_IR`) when the backend reports it.
    pub storage_class: Option<String>,
}

/// The minimal object-store surface Arkstore needs. A concrete S3 backend lands
/// in M0; the operations depend only on this trait.
pub trait ObjectStore {
    /// List every object under `prefix` (paginated internally).
    fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>>;
    /// Upload `bytes` to `key`.
    fn put(&self, key: &str, bytes: &[u8]) -> Result<()>;
    /// Download the object at `key`.
    fn get(&self, key: &str) -> Result<Vec<u8>>;
    /// Delete a batch of keys.
    fn delete(&self, keys: &[String]) -> Result<()>;
}
