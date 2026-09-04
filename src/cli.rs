//! Command-line interface (PRD §10): one subcommand per operation, positional
//! sub-actions where an operation has them, consistent flags.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use crate::config::{SourceType, TargetOverrides};
use crate::ops::CleanupAction;

/// Backup, restore, retention-cleanup, cold-tier archival, and verification
/// for databases and files against object storage.
#[derive(Debug, Parser)]
#[command(name = "arkstore", version, about, long_about = None)]
pub struct Cli {
    /// Path to the Arkstore config file.
    #[arg(short, long, global = true, default_value = "arkstore.yaml")]
    pub config: PathBuf,

    /// Log level: error | warn | info | debug | trace (overridden by RUST_LOG).
    #[arg(long, global = true)]
    pub log_level: Option<String>,

    /// Override `app.timezone` (IANA name) for this run.
    #[arg(long, global = true)]
    pub timezone: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

/// Restore sub-actions (positional, like `cleanup`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum RestoreAction {
    /// Restore the selected backup into the target (default).
    #[default]
    Restore,
    /// List the versioned backups available for the source; write nothing.
    ListBackups,
}

/// Per-field target overrides shared by `restore` and `verify`. The password
/// is deliberately not a flag (see `ARKSTORE_TARGET_PASSWORD`).
#[derive(Debug, Clone, Default, clap::Args)]
pub struct TargetArgs {
    /// Named `targets` entry to restore into (default: the source's own name;
    /// env: ARKSTORE_TARGET).
    #[arg(long)]
    pub target: Option<String>,
    /// Override the target host (env: ARKSTORE_TARGET_HOST).
    #[arg(long)]
    pub target_host: Option<String>,
    /// Override the target port (env: ARKSTORE_TARGET_PORT).
    #[arg(long)]
    pub target_port: Option<u16>,
    /// Override the target database (env: ARKSTORE_TARGET_DB).
    #[arg(long)]
    pub target_db: Option<String>,
    /// Override the target user (env: ARKSTORE_TARGET_USER).
    #[arg(long)]
    pub target_user: Option<String>,
    /// Override the target path for file sources (env: ARKSTORE_TARGET_PATH).
    #[arg(long)]
    pub target_path: Option<String>,
}

impl From<&TargetArgs> for TargetOverrides {
    fn from(a: &TargetArgs) -> Self {
        Self {
            target: a.target.clone(),
            host: a.target_host.clone(),
            port: a.target_port,
            database: a.target_db.clone(),
            user: a.target_user.clone(),
            path: a.target_path.clone(),
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Back up databases / file trees to object storage.
    Backup {
        /// Limit to one engine type.
        #[arg(long = "type", value_enum)]
        kind: Option<SourceType>,
        /// Limit to a single source by name.
        #[arg(long)]
        source: Option<String>,
        /// Report what would happen; write and delete nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Restore one source from a backup into a target.
    Restore {
        #[arg(value_enum, default_value_t = RestoreAction::Restore)]
        action: RestoreAction,
        /// The source to restore (required).
        #[arg(long)]
        source: String,
        /// Which backup: `latest` (default), a stamp / object key, or a local
        /// dump path.
        #[arg(long, default_value = "latest")]
        from: String,
        #[command(flatten)]
        target: TargetArgs,
        /// Run every check and compute the load order; write nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Apply calendar-tier retention to stored backups.
    Cleanup {
        /// Which cleanup action to run.
        #[arg(value_enum, default_value_t = CleanupAction::Run)]
        action: CleanupAction,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Move aged rows from a live DB into Parquet in object storage.
    Archive {
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Prove a backup is restorable: round-trip it into a throwaway target and
    /// diff it against the manifest baseline.
    Verify {
        /// The source whose backup to verify (required).
        #[arg(long)]
        source: String,
        #[arg(long, default_value = "latest")]
        from: String,
        #[command(flatten)]
        target: TargetArgs,
        /// Report what would be verified; restore nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_backup_cleanup_and_globals() {
        let c =
            Cli::try_parse_from(["arkstore", "backup", "--type", "postgre", "--dry-run"]).unwrap();
        assert!(matches!(
            c.command,
            Command::Backup {
                kind: Some(SourceType::Postgre),
                dry_run: true,
                ..
            }
        ));
        let c = Cli::try_parse_from([
            "arkstore",
            "--timezone",
            "Europe/Berlin",
            "cleanup",
            "generate-plan",
        ])
        .unwrap();
        assert_eq!(c.timezone.as_deref(), Some("Europe/Berlin"));
        assert!(matches!(
            c.command,
            Command::Cleanup {
                action: CleanupAction::GeneratePlan,
                ..
            }
        ));
    }

    #[test]
    fn parses_restore_and_verify_shapes() {
        let c = Cli::try_parse_from([
            "arkstore",
            "restore",
            "list-backups",
            "--source",
            "appdb",
            "--target",
            "staging",
            "--target-port",
            "6543",
        ])
        .unwrap();
        match c.command {
            Command::Restore {
                action,
                source,
                from,
                target,
                ..
            } => {
                assert_eq!(action, RestoreAction::ListBackups);
                assert_eq!(source, "appdb");
                assert_eq!(from, "latest");
                assert_eq!(target.target.as_deref(), Some("staging"));
                assert_eq!(target.target_port, Some(6543));
            }
            other => panic!("unexpected {other:?}"),
        }
        let c = Cli::try_parse_from(["arkstore", "restore", "--source", "appdb"]).unwrap();
        assert!(matches!(
            c.command,
            Command::Restore {
                action: RestoreAction::Restore,
                ..
            }
        ));
        let c = Cli::try_parse_from([
            "arkstore",
            "verify",
            "--source",
            "appdb",
            "--from",
            "2026-09-04-074507",
        ])
        .unwrap();
        assert!(matches!(c.command, Command::Verify { .. }));
    }

    #[test]
    fn restore_and_verify_require_source_and_have_no_password_flag() {
        assert!(Cli::try_parse_from(["arkstore", "restore"]).is_err());
        assert!(Cli::try_parse_from(["arkstore", "verify"]).is_err());
        assert!(Cli::try_parse_from([
            "arkstore",
            "restore",
            "--source",
            "a",
            "--target-password",
            "x"
        ])
        .is_err());
    }
}
