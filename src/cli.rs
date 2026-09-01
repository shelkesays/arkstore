//! Command-line interface: one subcommand per operation, consistent flags.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::ops::CleanupAction;

/// Backup, restore, retention-cleanup, and cold-tier archival to object storage.
#[derive(Debug, Parser)]
#[command(name = "arkstore", version, about, long_about = None)]
pub struct Cli {
    /// Path to the Arkstore config file.
    #[arg(short, long, global = true, default_value = "arkstore.yaml")]
    pub config: PathBuf,

    /// Log level: error | warn | info | debug | trace (overridden by RUST_LOG).
    #[arg(long, global = true)]
    pub log_level: Option<String>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Back up databases / file trees to object storage.
    Backup {
        /// Limit to a single source by name.
        #[arg(long)]
        source: Option<String>,
        /// Report what would happen; write and delete nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Restore a database / file tree from a backup.
    Restore {
        #[arg(long)]
        source: Option<String>,
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
}
