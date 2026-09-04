#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use tracing::{debug, error, warn};
use tracing_subscriber::EnvFilter;

use arkstore::cli::{Cli, Command};
use arkstore::config::{Config, ProcessEnv};
use arkstore::error::ArkError;
use arkstore::ops::{self, RestoreRequest, VerifyRequest};
use arkstore::secrets;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.log_level.as_deref());
    install_crypto_provider();

    // Ctrl-C cancels in-flight work cooperatively; ops clean up via Drop
    // guards and the process exits 130 (PRD §9.6).
    let outcome = tokio::select! {
        result = run(&cli) => result,
        _ = tokio::signal::ctrl_c() => Err(ArkError::Interrupted),
    };

    match outcome {
        Ok(failed) if failed.is_empty() => ExitCode::SUCCESS,
        Ok(failed) => {
            error!(failed = ?failed, "one or more items failed");
            ExitCode::FAILURE
        }
        Err(ArkError::Interrupted) => {
            warn!("interrupted; cleaning up");
            ExitCode::from(ArkError::Interrupted.exit_code())
        }
        Err(err) => {
            error!(%err, "arkstore failed");
            ExitCode::from(err.exit_code())
        }
    }
}

/// Load config + secrets and dispatch. Returns the names of items that
/// failed so `main` can pick the exit code.
async fn run(cli: &Cli) -> arkstore::Result<Vec<String>> {
    // Read -> CLI overrides -> secrets -> validate, so a host/user that lives
    // only in the secrets file, or a --timezone override, is in place before
    // anything is checked.
    let mut config = Config::load_unvalidated(&cli.config)?;
    if let Some(tz) = &cli.timezone {
        config.app.timezone = tz.clone();
    }
    secrets::load_secrets(&mut config, &ProcessEnv)?;
    config.validate()?;

    match &cli.command {
        Command::Backup {
            kind,
            source,
            dry_run,
        } => ops::backup(&config, *kind, source.as_deref(), *dry_run).await,
        Command::Restore {
            action,
            source,
            from,
            target,
            dry_run,
        } => {
            let request = RestoreRequest {
                source: source.clone(),
                action: *action,
                from: from.clone(),
                target: target.into(),
            };
            ops::restore(&config, &request, *dry_run).await
        }
        Command::Cleanup {
            action,
            source,
            dry_run,
        } => ops::cleanup(&config, *action, source.as_deref(), *dry_run).await,
        Command::Archive { source, dry_run } => {
            ops::archive(&config, source.as_deref(), *dry_run).await
        }
        Command::Verify {
            source,
            from,
            target,
            dry_run,
        } => {
            let request = VerifyRequest {
                source: source.clone(),
                from: from.clone(),
                target: target.into(),
            };
            ops::verify(&config, &request, *dry_run).await
        }
    }
}

/// rustls is built without a bundled provider (so the S3 client links `ring`,
/// not aws-lc-rs); make `ring` the process default before any TLS handshake.
fn install_crypto_provider() {
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        debug!("rustls crypto provider was already installed");
    }
}

fn init_tracing(level: Option<&str>) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level.unwrap_or("info")));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
