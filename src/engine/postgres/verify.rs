//! Native PostgreSQL verify (KB §12): re-introspect a restored target with
//! the same catalog reader and DDL emitter the dump used, and compare every
//! manifest object on both axes — `schema_hash` of the canonical definition,
//! and for tables the row count and order-independent content hash — plus
//! create / drop of the throwaway database Arkstore owns.

use std::collections::HashMap;

use tracing::{debug, info};

use super::catalog::Catalog;
use super::conn::Conn;
use super::ddl::Emitter;
use super::dump::{hash_table, rel_names, script_for, Item, Plan};
use super::sql::{qualified, quote};
use crate::config::{ResolvedTarget, Source};
use crate::engine::VerifyOutcome;
use crate::error::Result;
use crate::manifest::{Manifest, ObjectEntry};

/// Compare `target` (restored from `manifest`) with the manifest baseline.
pub async fn verify(
    source: &Source,
    target: &ResolvedTarget,
    manifest: &Manifest,
) -> Result<VerifyOutcome> {
    let conn = Conn::connect_target(target).await?;
    conn.begin_snapshot().await?;
    let catalog = Catalog::load(&conn, source.effective_include_privileges()).await?;
    let plan = Plan::build(source, &catalog);
    let emitter = Emitter {
        catalog: &catalog,
        include_privileges: source.effective_include_privileges(),
        rel_names: rel_names(&catalog),
    };
    let found: HashMap<&str, &Item> = plan.items.iter().map(|i| (i.name.as_str(), i)).collect();
    let mut outcome = VerifyOutcome::default();
    for expected in &manifest.objects {
        compare_object(&conn, &emitter, &found, expected, &mut outcome).await;
    }
    for item in &plan.items {
        if !manifest.objects.iter().any(|o| o.name == item.name) {
            outcome.mismatched.push((
                item.name.clone(),
                "present in the target but not in the manifest".into(),
            ));
        }
    }
    conn.end_snapshot().await?;
    info!(
        source = %source.name,
        target = %target.name,
        verified = outcome.verified.len(),
        mismatched = outcome.mismatched.len(),
        failed = outcome.failed.len(),
        "verify finished"
    );
    Ok(outcome)
}

/// Definition first, then data when the manifest recorded any.
async fn compare_object(
    conn: &Conn,
    emitter: &Emitter<'_>,
    found: &HashMap<&str, &Item>,
    expected: &ObjectEntry,
    outcome: &mut VerifyOutcome,
) {
    let Some(item) = found.get(expected.name.as_str()) else {
        outcome.mismatched.push((
            expected.name.clone(),
            "missing from the restored target".into(),
        ));
        return;
    };
    let actual = script_for(item, emitter).schema_hash();
    if actual != expected.schema_hash {
        outcome.mismatched.push((
            expected.name.clone(),
            "definition differs from the manifest (schema_hash)".into(),
        ));
        return;
    }
    match compare_data(conn, expected).await {
        Ok(None) => outcome.verified.push(expected.name.clone()),
        Ok(Some(reason)) => outcome.mismatched.push((expected.name.clone(), reason)),
        Err(e) => outcome.failed.push((expected.name.clone(), e.to_string())),
    }
}

/// `Ok(None)` when the data matches (or the object carries none),
/// `Ok(Some(reason))` on a count or content-hash difference.
async fn compare_data(conn: &Conn, expected: &ObjectEntry) -> Result<Option<String>> {
    let Some(rows) = expected.row_count else {
        return Ok(None);
    };
    let (schema, name) = expected
        .name
        .split_once('.')
        .unwrap_or(("public", &expected.name));
    let sum = hash_table(conn, &qualified(schema, name)).await?;
    if sum.rows() != rows {
        return Ok(Some(format!(
            "row count {} differs from the manifest's {rows}",
            sum.rows()
        )));
    }
    let hash = sum.finish();
    if Some(hash.as_str()) != expected.content_hash.as_deref() {
        return Ok(Some("content hash differs from the manifest".into()));
    }
    debug!(object = %expected.name, rows, "data verified");
    Ok(None)
}

/// `CREATE DATABASE` on the verify server (its own connection; the
/// statement cannot run inside a transaction block, so it goes alone).
pub async fn create_database(server: &ResolvedTarget, name: &str) -> Result<()> {
    let conn = Conn::connect_target(server).await?;
    conn.exec(&format!("CREATE DATABASE {}", quote(name)))
        .await?;
    info!(database = name, server = %server.name, "created throwaway database");
    Ok(())
}

/// `DROP DATABASE … WITH (FORCE)` — only ever for a database Arkstore created.
pub async fn drop_database(server: &ResolvedTarget, name: &str) -> Result<()> {
    let conn = Conn::connect_target(server).await?;
    conn.exec(&format!(
        "DROP DATABASE IF EXISTS {} WITH (FORCE)",
        quote(name)
    ))
    .await?;
    info!(database = name, server = %server.name, "dropped throwaway database");
    Ok(())
}
