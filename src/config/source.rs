//! Source definitions: databases and file trees Arkstore acts on.

use serde::Deserialize;

/// The family a source belongs to. Serialized lowercase (`postgre`, `mysql`, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceType {
    Postgre,
    Mysql,
    Mongo,
    File,
}

impl SourceType {
    /// Whether this source can be archived (databases yes, files no).
    pub fn is_archivable(self) -> bool {
        matches!(self, Self::Postgre | Self::Mysql | Self::Mongo)
    }
}

/// One archive rule: a table/collection to age out on a timestamp column.
#[derive(Debug, Clone, Deserialize)]
pub struct ArchiveRule {
    /// SQL table name, or Mongo collection name.
    pub table: String,
    /// The timestamp column (SQL) or BSON date field (Mongo) to partition on.
    pub time_column: String,
    /// Rows older than this many days are archived. Falls back to
    /// `archive.default_retention_days` when omitted.
    #[serde(default)]
    pub retention_days: Option<u32>,
}

fn default_enable() -> bool {
    true
}

/// A backup / restore / archive source.
#[derive(Debug, Clone, Deserialize)]
pub struct Source {
    pub name: String,
    #[serde(rename = "type")]
    pub source_type: SourceType,
    #[serde(default = "default_enable")]
    pub enable: bool,

    // Connection details (databases). Populated from config and/or secrets.
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub password: Option<String>,

    /// For `file` sources: the path tree to snapshot.
    #[serde(default)]
    pub path: Option<String>,

    /// Archive rules. Empty or absent means `archive` skips this source.
    #[serde(default)]
    pub archive: Vec<ArchiveRule>,
}
