//! Verify: prove a backup is restorable by round-tripping it into a throwaway
//! target and diffing it against the manifest baseline (PRD §6.5, KB §12).
//!
//! The target is either a `targets` entry flagged `ephemeral: true` (used as
//! is, never dropped) or a database Arkstore creates on `verify.server` and
//! always drops — on success, failure, or Ctrl-C.

use chrono::Utc;
use tracing::{error, info, warn};

use crate::config::{
    check_not_production, resolve_target, Config, ProcessEnv, ResolvedTarget, Source, Target,
    TargetOverrides,
};
use crate::engine::{
    create_database, drop_database, ensure_engine, verify_database, VerifyOutcome,
};
use crate::error::{ArkError, Result};
use crate::layout::render_stamp;
use crate::ops::restore::restore_database_to;
use crate::store::Store;

/// What the user asked `verify` to do.
#[derive(Debug, Clone)]
pub struct VerifyRequest {
    pub source: String,
    pub from: String,
    pub target: TargetOverrides,
}

/// Longest source-name fragment kept in a created database name, so the
/// whole name stays under PostgreSQL's 63-byte identifier limit.
const NAME_FRAGMENT: usize = 24;

/// Where the round trip lands.
enum Throwaway {
    /// A pre-existing `ephemeral: true` target — never dropped.
    Ephemeral(ResolvedTarget),
    /// A database Arkstore creates on the verify server and always drops.
    Created {
        server: ResolvedTarget,
        target: ResolvedTarget,
    },
}

impl Throwaway {
    fn target(&self) -> &ResolvedTarget {
        match self {
            Self::Ephemeral(t) | Self::Created { target: t, .. } => t,
        }
    }
}

/// Verify one source's backup against the configured store.
pub async fn run(config: &Config, request: &VerifyRequest, dry_run: bool) -> Result<Vec<String>> {
    let store = Store::from_config(config)?;
    run_with_store(config, &store, request, dry_run).await
}

/// [`run`] against an explicit store. Returns the source name when anything
/// mismatched or failed, so the run exits `1`.
pub async fn run_with_store(
    config: &Config,
    store: &Store,
    request: &VerifyRequest,
    dry_run: bool,
) -> Result<Vec<String>> {
    let source = config.source(&request.source)?;
    ensure_engine(source.source_type)?;
    let throwaway = choose_target(config, source, &request.target)?;
    check_not_production(source, throwaway.target())?;
    if dry_run {
        return report_dry_run(config, store, source, &request.from, &throwaway).await;
    }
    if let Throwaway::Created { server, target } = &throwaway {
        create_database(server, target.database.as_deref().unwrap_or_default()).await?;
    }
    // Ctrl-C must still reach the teardown below, so the round trip is
    // raced here rather than in `main`.
    let result = tokio::select! {
        result = round_trip(config, store, source, throwaway.target(), &request.from) => result,
        _ = tokio::signal::ctrl_c() => Err(ArkError::Interrupted),
    };
    tear_down(&throwaway).await;
    let outcome = result?;
    Ok(report(source, &outcome))
}

/// Restore, then compare. The restore must itself be clean before the
/// comparison means anything.
async fn round_trip(
    config: &Config,
    store: &Store,
    source: &Source,
    target: &ResolvedTarget,
    from: &str,
) -> Result<VerifyOutcome> {
    let done = restore_database_to(config, store, source, target, from, false)
        .await?
        .ok_or_else(|| ArkError::Internal("restore returned nothing outside a dry run".into()))?;
    let mut outcome = verify_database(source, target, &done.manifest).await?;
    for (object, reason) in done.outcome.failed {
        outcome.failed.push((object, format!("restore: {reason}")));
    }
    Ok(outcome)
}

/// Drop only what Arkstore created; a failure to drop is loud, never masked.
async fn tear_down(throwaway: &Throwaway) {
    let Throwaway::Created { server, target } = throwaway else {
        return;
    };
    let name = target.database.as_deref().unwrap_or_default();
    if let Err(e) = drop_database(server, name).await {
        error!(database = name, error = %e, "could not drop the throwaway database; drop it by hand");
    }
}

/// `--target` / env name an `ephemeral: true` entry; otherwise `verify.server`
/// creates a database; otherwise the source-named entry, if ephemeral.
fn choose_target(
    config: &Config,
    source: &Source,
    overrides: &TargetOverrides,
) -> Result<Throwaway> {
    let explicit = overrides.target.is_some() || std::env::var_os("ARKSTORE_TARGET").is_some();
    if !explicit {
        if let Some(server) = &config.verify.server {
            return created_target(config, source, server);
        }
    }
    let target = resolve_target(config, source, overrides, &ProcessEnv)?;
    if !target.ephemeral {
        return Err(ArkError::Refused(format!(
            "target `{}` is not flagged `ephemeral: true` — verify only restores into a throwaway target, or creates one on `verify.server`",
            target.name
        )));
    }
    Ok(Throwaway::Ephemeral(target))
}

/// The server connection (its maintenance database) and the database
/// Arkstore will create on it: `arkstore_verify_<source>_<stamp>`.
fn created_target(config: &Config, source: &Source, server: &Target) -> Result<Throwaway> {
    let stamp = render_stamp(Utc::now(), config.timezone()?);
    let fragment: String = source
        .name
        .chars()
        .take(NAME_FRAGMENT)
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let name = format!("arkstore_verify_{fragment}_{stamp}");
    let mut server_target = resolved_from_entry(server, "postgres");
    server_target.database = server.database.clone().or(Some("postgres".into()));
    let mut target = resolved_from_entry(server, &name);
    target.name = format!("verify:{name}");
    target.ephemeral = true;
    Ok(Throwaway::Created {
        server: server_target,
        target,
    })
}

fn resolved_from_entry(entry: &Target, database: &str) -> ResolvedTarget {
    ResolvedTarget {
        name: entry.name.clone(),
        kind: entry.target_type,
        host: entry.host.clone(),
        port: entry.port.unwrap_or(entry.target_type.default_port()),
        database: Some(database.to_string()),
        user: entry.user.clone(),
        password: entry.password.clone(),
        auth_db: entry.auth_db.clone(),
        path: None,
        ephemeral: true,
        tls: entry.tls,
        tls_ca_file: entry.tls_ca_file.clone(),
    }
}

async fn report_dry_run(
    config: &Config,
    store: &Store,
    source: &Source,
    from: &str,
    throwaway: &Throwaway,
) -> Result<Vec<String>> {
    match throwaway {
        Throwaway::Ephemeral(target) => {
            restore_database_to(config, store, source, target, from, true).await?;
        }
        Throwaway::Created { server, target } => {
            info!(source = %source.name, server = %server.name, database = target.database.as_deref().unwrap_or_default(), from, "dry run: would create the database, restore into it, compare, and drop it");
        }
    }
    Ok(vec![])
}

fn report(source: &Source, outcome: &VerifyOutcome) -> Vec<String> {
    for (object, reason) in &outcome.mismatched {
        warn!(source = %source.name, object, reason, "mismatch");
    }
    for (object, reason) in &outcome.failed {
        warn!(source = %source.name, object, reason, "could not verify");
    }
    info!(
        source = %source.name,
        verified = outcome.verified.len(),
        mismatched = outcome.mismatched.len(),
        failed = outcome.failed.len(),
        "verify summary"
    );
    if outcome.is_clean() {
        vec![]
    } else {
        vec![source.name.clone()]
    }
}
