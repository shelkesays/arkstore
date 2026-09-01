#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use tracing::{debug, error};
use tracing_subscriber::EnvFilter;

use arkstore::cli::{Cli, Command};
use arkstore::config::Config;
use arkstore::ops;

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.log_level.as_deref());

    match run(&cli) {
        Ok(failed) if failed.is_empty() => ExitCode::SUCCESS,
        Ok(failed) => {
            error!(failed = ?failed, "one or more items failed");
            ExitCode::FAILURE
        }
        Err(err) => {
            error!(%err, "arkstore failed");
            ExitCode::FAILURE
        }
    }
}

/// Load config and dispatch to the requested operation. Returns the names of
/// items (sources/targets) that failed, so `main` can pick the exit code.
fn run(cli: &Cli) -> arkstore::Result<Vec<String>> {
    let config = Config::load(&cli.config)?;

    let concurrency = config.concurrency.resolved();
    debug!(
        max_sources = concurrency.max_sources,
        cpu_workers = concurrency.cpu_workers,
        "resolved concurrency"
    );

    match &cli.command {
        Command::Backup { source, dry_run } => ops::backup(&config, source.as_deref(), *dry_run),
        Command::Restore { source, dry_run } => ops::restore(&config, source.as_deref(), *dry_run),
        Command::Cleanup {
            action,
            source,
            dry_run,
        } => ops::cleanup(&config, *action, source.as_deref(), *dry_run),
        Command::Archive { source, dry_run } => ops::archive(&config, source.as_deref(), *dry_run),
    }
}

fn init_tracing(level: Option<&str>) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level.unwrap_or("info")));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
