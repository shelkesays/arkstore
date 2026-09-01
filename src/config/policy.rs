//! Global per-operation policy blocks (`cleanup:` and `archive:`).

use serde::Deserialize;

fn default_true() -> bool {
    true
}

/// Which retention bands are active during cleanup. A disabled tier folds into
/// the next-coarser one rather than being deleted wholesale.
#[derive(Debug, Clone, Deserialize)]
pub struct RetentionTiers {
    #[serde(default = "default_true")]
    pub daily: bool,
    #[serde(default = "default_true")]
    pub weekly: bool,
    #[serde(default = "default_true")]
    pub monthly: bool,
    #[serde(default = "default_true")]
    pub yearly: bool,
}

impl Default for RetentionTiers {
    fn default() -> Self {
        Self {
            daily: true,
            weekly: true,
            monthly: true,
            yearly: true,
        }
    }
}

/// The `cleanup:` policy block.
#[derive(Debug, Clone, Deserialize)]
pub struct CleanupPolicy {
    #[serde(default = "default_true")]
    pub enable: bool,
    #[serde(default)]
    pub retention: RetentionTiers,
    /// Prefix under which plans/reports and their consolidated audit files live.
    #[serde(default = "default_plans_prefix")]
    pub plans_prefix: String,
    /// Objects deleted per batch request (S3 maximum is 1000).
    #[serde(default = "default_delete_batch_size")]
    pub delete_batch_size: usize,
    #[serde(default = "default_true")]
    pub consolidate_plans: bool,
}

fn default_plans_prefix() -> String {
    "retention-plans/".to_string()
}

fn default_delete_batch_size() -> usize {
    1000
}

impl Default for CleanupPolicy {
    fn default() -> Self {
        Self {
            enable: true,
            retention: RetentionTiers::default(),
            plans_prefix: default_plans_prefix(),
            delete_batch_size: default_delete_batch_size(),
            consolidate_plans: true,
        }
    }
}

/// The `archive:` policy block.
#[derive(Debug, Clone, Deserialize)]
pub struct ArchivePolicy {
    #[serde(default = "default_true")]
    pub enable: bool,
    /// Output format for archived partitions (currently `parquet`).
    #[serde(default = "default_format")]
    pub format: String,
    /// Dedicated top-level prefix for archives, outside `aws.folder`.
    #[serde(default = "default_archive_prefix")]
    pub s3_prefix: String,
    /// Fallback when an archive rule omits `retention_days`.
    #[serde(default = "default_retention_days")]
    pub default_retention_days: u32,
    /// Snap a mid-month cutoff back to the first of the month (archive whole
    /// calendar months only).
    #[serde(default = "default_true")]
    pub whole_months: bool,
    /// Delete source rows after a month's partition is uploaded and verified.
    #[serde(default = "default_true")]
    pub delete_after_archive: bool,
    #[serde(default)]
    pub dry_run: bool,
    /// Parquet compression codec.
    #[serde(default = "default_compression")]
    pub compression: String,
    /// Rows fetched per batch when streaming a partition to Parquet.
    #[serde(default = "default_fetch_batch_size")]
    pub fetch_batch_size: usize,
}

fn default_format() -> String {
    "parquet".to_string()
}

fn default_archive_prefix() -> String {
    "archive".to_string()
}

fn default_retention_days() -> u32 {
    90
}

fn default_compression() -> String {
    "snappy".to_string()
}

fn default_fetch_batch_size() -> usize {
    50_000
}

impl Default for ArchivePolicy {
    fn default() -> Self {
        Self {
            enable: true,
            format: default_format(),
            s3_prefix: default_archive_prefix(),
            default_retention_days: default_retention_days(),
            whole_months: true,
            delete_after_archive: true,
            dry_run: false,
            compression: default_compression(),
            fetch_batch_size: default_fetch_batch_size(),
        }
    }
}
