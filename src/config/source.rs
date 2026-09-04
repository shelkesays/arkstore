//! Source definitions: the databases and file trees Arkstore acts on.

use clap::ValueEnum;
use serde::{Deserialize, Serialize};

use crate::config::name::validate_name;
use crate::error::{ArkError, Result};
use crate::secrets::Secret;

/// The family a source (or target) belongs to. Serialized lowercase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
#[value(rename_all = "lowercase")]
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

    /// Whether this is a database (as opposed to a file tree).
    pub fn is_database(self) -> bool {
        self.is_archivable()
    }

    /// The engine's conventional port.
    pub fn default_port(self) -> u16 {
        match self {
            Self::Postgre => 5432,
            Self::Mysql => 3306,
            Self::Mongo => 27017,
            Self::File => 0,
        }
    }

    /// Human-readable engine name for messages.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Postgre => "PostgreSQL",
            Self::Mysql => "MySQL",
            Self::Mongo => "MongoDB",
            Self::File => "file",
        }
    }
}

/// On-archive data encoding for Postgres tables (PRD §5.1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CopyFormat {
    /// `COPY … TO STDOUT` text format — portable, the default.
    #[default]
    Text,
    /// Binary `COPY` — opt-in, same-version/same-architecture only.
    Binary,
}

/// TLS negotiation mode for a database connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TlsMode {
    Disable,
    #[default]
    Prefer,
    Require,
    VerifyFull,
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

fn default_true() -> bool {
    true
}

/// A backup / restore / archive source. Per-engine defaults are applied by
/// the `effective_*` accessors, not at deserialization, so "unset" stays
/// distinguishable from "explicitly set".
#[derive(Debug, Clone, Deserialize)]
pub struct Source {
    pub name: String,
    #[serde(rename = "type")]
    pub source_type: SourceType,
    #[serde(default = "default_true")]
    pub enable: bool,

    // ---- connection (databases); password is merged from secrets ----
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    /// Database name. Defaults to the source name.
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub password: Option<Secret>,
    /// Mongo: the authentication database. Defaults to `database`.
    #[serde(default)]
    pub authentication_database: Option<String>,
    #[serde(default)]
    pub tls: TlsMode,
    /// Optional CA bundle (PEM) for `verify-full`.
    #[serde(default)]
    pub tls_ca_file: Option<String>,

    // ---- what to dump ----
    /// Dump object definitions (DDL). Default: true, except Mongo (false).
    #[serde(default)]
    pub structure: Option<bool>,
    /// Dump rows / documents. Default: true.
    #[serde(default = "default_true")]
    pub data: bool,
    /// Object-name prefixes excluded outright. Engine defaults apply when unset.
    #[serde(default)]
    pub ignore_startswith: Option<Vec<String>>,
    /// Named objects to skip (Postgres: data only, structure kept).
    #[serde(default)]
    pub ignore: Option<Vec<String>>,
    /// File sources: extensions to skip.
    #[serde(default)]
    pub ignore_extensions: Option<Vec<String>>,
    /// Emit ownership / privileges in DDL (Postgres, MySQL). Default: false.
    #[serde(default)]
    pub include_privileges: Option<bool>,
    /// Postgres data encoding. Default: text.
    #[serde(default)]
    pub copy_format: Option<CopyFormat>,
    /// MySQL: dump non-transactional tables outside the snapshot instead of
    /// failing the source. Default: false.
    #[serde(default)]
    pub allow_unsnapshotted_tables: Option<bool>,

    // ---- where it goes ----
    #[serde(default = "default_true")]
    pub backup_to_s3: bool,
    #[serde(default = "default_true")]
    pub delete_after_upload: bool,
    /// Newest local versioned archives to keep; `0` disables pruning.
    #[serde(default)]
    pub local_retention: u32,

    /// File sources: the path tree to snapshot.
    #[serde(default)]
    pub path: Option<String>,

    /// Archive rules. Empty or absent means `archive` skips this source.
    #[serde(default)]
    pub archive: Vec<ArchiveRule>,
}

impl Source {
    /// The database name (defaults to the source name).
    pub fn database(&self) -> &str {
        self.database.as_deref().unwrap_or(&self.name)
    }

    /// Mongo authentication database (defaults to [`Self::database`]).
    pub fn auth_database(&self) -> &str {
        self.authentication_database
            .as_deref()
            .unwrap_or_else(|| self.database())
    }

    /// Effective port: explicit, else the engine default.
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(self.source_type.default_port())
    }

    /// Emit ownership / privileges in DDL: explicit, else `false`. Stored as
    /// `Option` (like `copy_format` / `allow_unsnapshotted_tables`) so an
    /// explicit value on an engine it does not apply to is rejected, whatever
    /// the value.
    pub fn effective_include_privileges(&self) -> bool {
        self.include_privileges.unwrap_or(false)
    }

    /// Postgres data encoding: explicit, else text.
    pub fn effective_copy_format(&self) -> CopyFormat {
        self.copy_format.unwrap_or_default()
    }

    /// MySQL: dump non-transactional tables outside the snapshot: explicit,
    /// else `false` (fail the source).
    pub fn effective_allow_unsnapshotted_tables(&self) -> bool {
        self.allow_unsnapshotted_tables.unwrap_or(false)
    }

    /// Whether structure (DDL) is dumped: explicit, else engine default.
    pub fn effective_structure(&self) -> bool {
        self.structure
            .unwrap_or(!matches!(self.source_type, SourceType::Mongo))
    }

    /// `ignore_startswith`: explicit, else the engine's system prefixes.
    pub fn effective_ignore_startswith(&self) -> Vec<String> {
        self.ignore_startswith.clone().unwrap_or_else(|| {
            match self.source_type {
                SourceType::Postgre => vec!["pg_", "rds_", "awsdms_"],
                SourceType::Mongo => vec!["system.", "local."],
                SourceType::Mysql | SourceType::File => vec![],
            }
            .into_iter()
            .map(str::to_string)
            .collect()
        })
    }

    /// `ignore`: explicit, else the engine's system objects.
    pub fn effective_ignore(&self) -> Vec<String> {
        self.ignore
            .clone()
            .unwrap_or_else(|| match self.source_type {
                SourceType::Mongo => vec![
                    "system.profile".to_string(),
                    "local.startup_log".to_string(),
                ],
                _ => vec![],
            })
    }

    /// `ignore_extensions` (file sources): explicit, else common junk.
    pub fn effective_ignore_extensions(&self) -> Vec<String> {
        self.ignore_extensions
            .clone()
            .unwrap_or_else(|| match self.source_type {
                SourceType::File => ["photoslibrary", "DS_Store", "localized"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                _ => vec![],
            })
    }

    /// Reject a source that would fail an operation up front; the error names
    /// the source and field.
    pub fn validate(&self) -> Result<()> {
        validate_name("source", &self.name)?;
        self.validate_connection()?;
        self.validate_engine_specific()?;
        self.validate_archive_rules()
    }

    fn field_error(&self, field: &str, why: &str) -> ArkError {
        ArkError::Validation(format!("source `{}`: `{field}` {why}", self.name))
    }

    fn validate_connection(&self) -> Result<()> {
        if self.source_type.is_database() {
            if is_blank(&self.host) {
                return Err(self.field_error("host", "is required for database sources"));
            }
            if is_blank(&self.user) {
                return Err(self.field_error("user", "is required for database sources"));
            }
            if self.path.is_some() {
                return Err(self.field_error("path", "applies to file sources only"));
            }
            return Ok(());
        }
        if is_blank(&self.path) {
            return Err(self.field_error("path", "is required for file sources"));
        }
        if !self.archive.is_empty() {
            return Err(self.field_error("archive", "rules apply to database sources only"));
        }
        Ok(())
    }

    /// Fields that only make sense for some engines: (field, is set, allowed
    /// on this engine, engines it applies to).
    fn validate_engine_specific(&self) -> Result<()> {
        let ty = self.source_type;
        let misuse = [
            (
                "copy_format",
                self.copy_format.is_some(),
                matches!(ty, SourceType::Postgre),
                "postgre",
            ),
            (
                "allow_unsnapshotted_tables",
                self.allow_unsnapshotted_tables.is_some(),
                matches!(ty, SourceType::Mysql),
                "mysql",
            ),
            (
                "include_privileges",
                self.include_privileges.is_some(),
                matches!(ty, SourceType::Postgre | SourceType::Mysql),
                "postgre/mysql",
            ),
            (
                "authentication_database",
                self.authentication_database.is_some(),
                matches!(ty, SourceType::Mongo),
                "mongo",
            ),
            (
                "ignore_extensions",
                self.ignore_extensions.is_some(),
                matches!(ty, SourceType::File),
                "file",
            ),
        ];
        for (field, set, allowed, engines) in misuse {
            if set && !allowed {
                return Err(self.field_error(field, &format!("applies to {engines} sources only")));
            }
        }
        self.warn_tls();
        Ok(())
    }

    fn warn_tls(&self) {
        if self.tls == TlsMode::VerifyFull
            && self.tls_ca_file.is_none()
            && self.source_type.is_database()
        {
            tracing::warn!(
                source = %self.name,
                "tls: verify-full without tls_ca_file uses the system trust store"
            );
        }
    }

    fn validate_archive_rules(&self) -> Result<()> {
        for rule in &self.archive {
            if rule.table.trim().is_empty() || rule.time_column.trim().is_empty() {
                return Err(
                    self.field_error("archive", "rules need both `table` and `time_column`")
                );
            }
            if rule.retention_days == Some(0) {
                return Err(self.field_error("archive", "retention_days must be >= 1"));
            }
        }
        Ok(())
    }
}

fn is_blank(value: &Option<String>) -> bool {
    value.as_deref().is_none_or(|v| v.trim().is_empty())
}
