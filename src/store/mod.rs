//! Object-store access over the `object_store` crate (PRD §9.4): S3 and
//! S3-compatible endpoints first, with a local-filesystem backend for tests
//! and an in-memory one for unit tests. Uploads stream in chunks and are
//! verified by a `HEAD` before they count as done.

use std::path::Path as FsPath;
use std::sync::Arc;

use bytes::BytesMut;
use chrono::{DateTime, Utc};
use futures::{StreamExt, TryStreamExt};
use object_store::aws::AmazonS3Builder;
use object_store::local::LocalFileSystem;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::{ObjectStoreExt, WriteMultipart};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info};

use crate::config::{AwsConfig, Config};
use crate::error::{ArkError, Result};
use crate::hash::hex;
use crate::redact::redact;

/// Bytes read from disk per upload chunk.
const UPLOAD_CHUNK: usize = 8 * 1024 * 1024;
/// Upload parts in flight at once (bounds memory to ~UPLOAD_CHUNK × this).
const UPLOAD_IN_FLIGHT: usize = 4;

/// One stored object as seen by a listing or `HEAD`.
#[derive(Debug, Clone)]
pub struct ObjectInfo {
    pub key: String,
    pub size: u64,
    pub last_modified: DateTime<Utc>,
    pub e_tag: Option<String>,
}

/// What an upload produced, after verification.
#[derive(Debug, Clone)]
pub struct UploadReport {
    pub key: String,
    pub size: u64,
    /// Bare hex SHA-256 of the bytes sent.
    pub sha256: String,
    pub e_tag: Option<String>,
}

/// A handle to one bucket / root.
#[derive(Clone)]
pub struct Store {
    inner: Arc<dyn object_store::ObjectStore>,
    label: String,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store").field("label", &self.label).finish()
    }
}

impl Store {
    /// S3 (or S3-compatible via `aws.endpoint`). Credentials come from the
    /// standard AWS environment, instance metadata, or a web-identity token —
    /// never from Arkstore config.
    pub fn s3(aws: &AwsConfig) -> Result<Self> {
        let mut builder = AmazonS3Builder::from_env()
            .with_bucket_name(&aws.bucket)
            .with_region(&aws.region);
        if let Some(endpoint) = &aws.endpoint {
            builder = builder
                .with_endpoint(endpoint)
                .with_allow_http(endpoint.starts_with("http://"));
        }
        let inner = builder
            .build()
            .map_err(|e| store_err("cannot configure S3 client", e))?;
        Ok(Self {
            inner: Arc::new(inner),
            label: format!("s3://{}", aws.bucket),
        })
    }

    /// A directory on the local filesystem acting as the bucket.
    pub fn local(root: &FsPath) -> Result<Self> {
        std::fs::create_dir_all(root)?;
        let inner = LocalFileSystem::new_with_prefix(root)
            .map_err(|e| store_err("cannot open local store", e))?;
        Ok(Self {
            inner: Arc::new(inner),
            label: format!("file://{}", root.display()),
        })
    }

    /// An in-memory bucket (unit tests).
    pub fn in_memory() -> Self {
        Self {
            inner: Arc::new(InMemory::new()),
            label: "memory://".to_string(),
        }
    }

    /// The store the configuration points at.
    pub fn from_config(config: &Config) -> Result<Self> {
        Self::s3(&config.aws)
    }

    /// Human-readable identity for logs.
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Every object under `prefix`.
    pub async fn list(&self, prefix: &str) -> Result<Vec<ObjectInfo>> {
        let path = parse_path(prefix)?;
        let mut stream = self.inner.list(Some(&path));
        let mut out = Vec::new();
        while let Some(item) = stream.next().await {
            let meta = item.map_err(|e| store_err("list failed", e))?;
            out.push(ObjectInfo {
                key: meta.location.to_string(),
                size: meta.size,
                last_modified: meta.last_modified,
                e_tag: meta.e_tag,
            });
        }
        Ok(out)
    }

    /// Metadata of one object.
    pub async fn head(&self, key: &str) -> Result<ObjectInfo> {
        let meta = self
            .inner
            .head(&parse_path(key)?)
            .await
            .map_err(|e| store_err(&format!("head `{key}` failed"), e))?;
        Ok(ObjectInfo {
            key: meta.location.to_string(),
            size: meta.size,
            last_modified: meta.last_modified,
            e_tag: meta.e_tag,
        })
    }

    /// Stream `file` to `key` in bounded chunks, hashing as it goes, then
    /// verify the stored size by `HEAD`. Verification failure is an error —
    /// an unverified upload never counts as a backup.
    pub async fn upload_file(&self, key: &str, file: &FsPath) -> Result<UploadReport> {
        let path = parse_path(key)?;
        let upload = self
            .inner
            .put_multipart(&path)
            .await
            .map_err(|e| store_err(&format!("cannot start upload of `{key}`"), e))?;
        let mut writer = WriteMultipart::new(upload);
        let clean = crate::pack::sanitize(file)?;
        let mut reader = tokio::fs::File::open(&clean).await?;
        let (sent, sha256) = stream_chunks(&mut reader, &mut writer).await?;
        let result = writer
            .finish()
            .await
            .map_err(|e| store_err(&format!("upload of `{key}` failed"), e))?;
        let stored = self.head(key).await?;
        if stored.size != sent {
            return Err(ArkError::Store(format!(
                "upload verification failed for `{key}`: sent {sent} bytes, store reports {}",
                stored.size
            )));
        }
        info!(key, size = sent, store = %self.label, "uploaded and verified");
        Ok(UploadReport {
            key: key.to_string(),
            size: sent,
            sha256,
            e_tag: result.e_tag,
        })
    }

    /// Stream `key` into `dest`, returning the byte count.
    pub async fn download_to_file(&self, key: &str, dest: &FsPath) -> Result<u64> {
        let result = self
            .inner
            .get(&parse_path(key)?)
            .await
            .map_err(|e| store_err(&format!("cannot fetch `{key}`"), e))?;
        let mut stream = result.into_stream();
        let mut out = tokio::fs::File::create(dest).await?;
        let mut total: u64 = 0;
        while let Some(chunk) = stream
            .try_next()
            .await
            .map_err(|e| store_err("download failed", e))?
        {
            out.write_all(&chunk).await?;
            total = total.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
        }
        out.flush().await?;
        debug!(key, bytes = total, "downloaded");
        Ok(total)
    }

    /// Server-side copy (used to rewrite the `latest` pointer).
    pub async fn copy(&self, from: &str, to: &str) -> Result<()> {
        self.inner
            .copy(&parse_path(from)?, &parse_path(to)?)
            .await
            .map_err(|e| store_err(&format!("copy `{from}` -> `{to}` failed"), e))
    }

    /// Delete `keys`, returning how many were deleted. Stops at the first
    /// failure so a partial batch is reported, never hidden.
    pub async fn delete_keys(&self, keys: &[String]) -> Result<usize> {
        let paths: Vec<object_store::Result<Path>> = keys
            .iter()
            .map(|k| Path::parse(k).map_err(Into::into))
            .collect();
        let mut stream = self
            .inner
            .delete_stream(futures::stream::iter(paths).boxed());
        let mut deleted = 0usize;
        while let Some(item) = stream.next().await {
            item.map_err(|e| store_err("delete failed", e))?;
            deleted = deleted.saturating_add(1);
        }
        Ok(deleted)
    }
}

/// Pump `reader` into `writer` in bounded chunks, hashing as it goes.
/// Returns (bytes sent, bare hex SHA-256).
async fn stream_chunks(
    reader: &mut tokio::fs::File,
    writer: &mut WriteMultipart,
) -> Result<(u64, String)> {
    let mut hasher = Sha256::new();
    let mut sent: u64 = 0;
    loop {
        let mut chunk = BytesMut::with_capacity(UPLOAD_CHUNK);
        let n = reader.read_buf(&mut chunk).await?;
        if n == 0 {
            break;
        }
        hasher.update(&chunk);
        sent = sent.saturating_add(u64::try_from(n).unwrap_or(u64::MAX));
        writer
            .wait_for_capacity(UPLOAD_IN_FLIGHT)
            .await
            .map_err(|e| store_err("upload stalled", e))?;
        writer.put(chunk.freeze());
    }
    Ok((sent, hex(&hasher.finalize())))
}

fn parse_path(key: &str) -> Result<Path> {
    Path::parse(key).map_err(|e| ArkError::Store(format!("invalid object key `{key}`: {e}")))
}

fn store_err(context: &str, err: object_store::Error) -> ArkError {
    ArkError::Store(redact(&format!("{context}: {err}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn upload_verifies_and_lists_and_copies() {
        let store = Store::in_memory();
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("blob.bin");
        let payload = vec![7u8; 3 * 1024 * 1024 + 123];
        std::fs::write(&file, &payload).unwrap();

        let report = store
            .upload_file("f/s/versioned/s.x.tar.gz", &file)
            .await
            .unwrap();
        assert_eq!(report.size, payload.len() as u64);
        assert_eq!(report.sha256, crate::hash::sha256_hex(&payload));

        store
            .copy("f/s/versioned/s.x.tar.gz", "f/s/s.latest.tar.gz")
            .await
            .unwrap();
        let listed = store.list("f/s/").await.unwrap();
        let mut keys: Vec<_> = listed.iter().map(|o| o.key.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["f/s/s.latest.tar.gz", "f/s/versioned/s.x.tar.gz"]);
        assert!(listed.iter().all(|o| o.size == payload.len() as u64));

        let dest = dir.path().join("down.bin");
        let n = store
            .download_to_file("f/s/s.latest.tar.gz", &dest)
            .await
            .unwrap();
        assert_eq!(n, payload.len() as u64);
        assert_eq!(std::fs::read(&dest).unwrap(), payload);

        let deleted = store
            .delete_keys(&["f/s/versioned/s.x.tar.gz".to_string()])
            .await
            .unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(store.list("f/s/versioned/").await.unwrap().len(), 0);
        assert!(store.head("f/s/versioned/s.x.tar.gz").await.is_err());
    }

    #[test]
    fn store_errors_are_redacted() {
        let err = store_err(
            "cannot configure S3 client",
            object_store::Error::Generic {
                store: "S3",
                source: "password=hunter2 at https://u:pw@h".into(),
            },
        );
        let text = err.to_string();
        assert!(
            !text.contains("hunter2") && !text.contains(":pw@"),
            "{text}"
        );
    }
}
