//! Live PostgreSQL verify test: dump the fixture, then `verify` it against a
//! throwaway database Arkstore creates and drops, an ephemeral target it
//! must leave alone, and a tampered archive it must reject. Runs only when
//! `ARKSTORE_TEST_PG` is set to `host:port:user:password:database`.

#![cfg(feature = "postgres")]

use std::fs;
use std::path::Path;

use arkstore::config::{Config, TargetOverrides};
use arkstore::ops::{backup, verify_with_store, VerifyRequest};
use arkstore::pack::{pack_dir, unpack};
use arkstore::store::Store;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// The tests share one server, so they run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Target {
    host: String,
    port: u16,
    user: String,
    password: String,
    database: String,
}

fn target() -> Option<Target> {
    let raw = std::env::var("ARKSTORE_TEST_PG").ok()?;
    let mut parts = raw.split(':');
    let host = parts.next()?.to_string();
    let port = parts.next()?.parse().ok()?;
    let user = parts.next()?.to_string();
    let password = parts.next()?.to_string();
    let database = parts.next()?.to_string();
    Some(Target {
        host,
        port,
        user,
        password,
        database,
    })
}

async fn client(
    t: &Target,
    database: &str,
) -> Result<tokio_postgres::Client, tokio_postgres::Error> {
    let mut config = tokio_postgres::Config::new();
    config
        .host(&t.host)
        .port(t.port)
        .user(&t.user)
        .password(&t.password)
        .dbname(database);
    let (client, connection) = config.connect(tokio_postgres::NoTls).await?;
    let _driver = tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

async fn count(t: &Target, database: &str, sql: &str) -> Result<i64, tokio_postgres::Error> {
    let row = client(t, database).await?.query_one(sql, &[]).await?;
    row.try_get(0)
}

async fn fresh_database(t: &Target, name: &str) -> Result<(), tokio_postgres::Error> {
    let admin = client(t, "postgres").await?;
    admin
        .simple_query(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
        .await?;
    admin
        .simple_query(&format!("CREATE DATABASE \"{name}\""))
        .await?;
    Ok(())
}

/// How the throwaway target is provided.
enum Mode {
    /// `verify.server`: Arkstore creates and drops the database.
    Server,
    /// A `targets` entry pointing at `db`, with the given `ephemeral` flag.
    Entry { db: String, ephemeral: bool },
}

fn write_config(base: &Path, t: &Target, mode: &Mode) -> arkstore::Result<Config> {
    let conn = [
        format!("    host: {}", t.host),
        format!("    port: {}", t.port),
        format!("    user: {}", t.user),
        format!("    password: {}", t.password),
        "    tls: disable".to_string(),
    ]
    .join("\n");
    let mut lines = vec![
        "app:".to_string(),
        "  timezone: UTC".to_string(),
        format!("  local_dir: {}", base.join("local").display()),
        "aws:\n  bucket: b\n  region: r\n  folder: dbbackup".to_string(),
        "sources:\n  - name: appdb\n    type: postgre".to_string(),
        conn.clone(),
        format!("    database: {}", t.database),
        "    ignore: ['public.skip_me']".to_string(),
    ];
    match mode {
        Mode::Server => {
            lines.push("verify:\n  server:\n    name: pgserver\n    type: postgre".to_string());
            lines.push(conn);
            lines.push("    db: postgres".to_string());
        }
        Mode::Entry { db, ephemeral } => {
            lines.push("targets:\n  - name: appdb\n    type: postgre".to_string());
            lines.push(conn);
            lines.push(format!("    db: {db}\n    ephemeral: {ephemeral}"));
        }
    }
    fs::write(base.join("arkstore.yaml"), lines.join("\n"))?;
    Config::load(&base.join("arkstore.yaml"))
}

fn request(from: &str) -> VerifyRequest {
    VerifyRequest {
        source: "appdb".into(),
        from: from.into(),
        target: TargetOverrides::default(),
    }
}

/// Fixture into the source, one backup into a local store.
async fn seed(
    base: &Path,
    t: &Target,
    mode: &Mode,
) -> Result<(Config, Store), Box<dyn std::error::Error>> {
    client(t, &t.database)
        .await?
        .batch_execute(include_str!("fixtures/postgres_fixture.sql"))
        .await?;
    let config = write_config(base, t, mode)?;
    let store = Store::local(&base.join("bucket"))?;
    let failed = backup::run_with_store(&config, Some(&store), None, None, false).await?;
    assert!(failed.is_empty(), "{failed:?}");
    Ok((config, store))
}

async fn throwaway_databases(t: &Target) -> Result<i64, tokio_postgres::Error> {
    count(
        t,
        "postgres",
        "SELECT count(*) FROM pg_database WHERE datname LIKE 'arkstore\\_verify\\_%'",
    )
    .await
}

#[tokio::test]
async fn verify_round_trips_a_clean_backup_and_drops_its_database() -> TestResult {
    let _serial = SERIAL.lock().await;
    let Some(t) = target() else {
        eprintln!("ARKSTORE_TEST_PG not set; skipping live PostgreSQL test");
        return Ok(());
    };
    let base = tempfile::tempdir()?;
    let (config, store) = seed(base.path(), &t, &Mode::Server).await?;

    let failed = verify_with_store(&config, &store, &request("latest"), true).await?;
    assert!(failed.is_empty(), "dry run: {failed:?}");
    assert_eq!(throwaway_databases(&t).await?, 0, "dry run creates nothing");

    let failed = verify_with_store(&config, &store, &request("latest"), false).await?;
    assert!(failed.is_empty(), "{failed:?}");
    assert_eq!(
        throwaway_databases(&t).await?,
        0,
        "throwaway database dropped"
    );
    Ok(())
}

/// Break the manifest two ways — a wrong row count and a wrong definition
/// hash — and repack; verify must report both and still drop its database.
fn tamper_manifest(
    base: &Path,
    original: &Path,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let work = base.join("work");
    unpack(original, &work)?;
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(work.join("manifest.json"))?)?;
    for object in manifest["objects"].as_array_mut().ok_or("objects")? {
        if object["name"] == "shop.customers" {
            object["row_count"] = serde_json::json!(99);
        }
        if object["name"] == "shop.mood" {
            object["schema_hash"] = serde_json::json!(format!("sha256:{}", "0".repeat(64)));
        }
    }
    fs::write(
        work.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    let tampered = base.join("tampered.tar.gz");
    pack_dir(&work, &tampered)?;
    Ok(tampered)
}

#[tokio::test]
async fn verify_reports_mismatches_and_still_drops_its_database() -> TestResult {
    let _serial = SERIAL.lock().await;
    let Some(t) = target() else {
        return Ok(());
    };
    let base = tempfile::tempdir()?;
    let (config, store) = seed(base.path(), &t, &Mode::Server).await?;
    let versioned = store.list("dbbackup/appdb/versioned/").await?;
    let key = versioned.first().ok_or("no versioned backup")?.key.clone();
    let original = base.path().join("original.tar.gz");
    store.download_to_file(&key, &original).await?;
    let tampered = tamper_manifest(base.path(), &original)?;

    let from = tampered.to_string_lossy().into_owned();
    let failed = verify_with_store(&config, &store, &request(&from), false).await?;
    assert_eq!(failed, vec!["appdb".to_string()]);
    assert_eq!(
        throwaway_databases(&t).await?,
        0,
        "dropped even on mismatch"
    );
    Ok(())
}

#[tokio::test]
async fn verify_uses_an_ephemeral_target_as_is_and_refuses_a_permanent_one() -> TestResult {
    let _serial = SERIAL.lock().await;
    let Some(t) = target() else {
        return Ok(());
    };
    let db = format!("{}_verify", t.database);
    let base = tempfile::tempdir()?;
    let ephemeral = Mode::Entry {
        db: db.clone(),
        ephemeral: true,
    };
    let (config, store) = seed(base.path(), &t, &ephemeral).await?;
    fresh_database(&t, &db).await?;

    let failed = verify_with_store(&config, &store, &request("latest"), false).await?;
    assert!(failed.is_empty(), "{failed:?}");
    let still_there = format!("SELECT count(*) FROM pg_database WHERE datname = '{db}'");
    assert_eq!(
        count(&t, "postgres", &still_there).await?,
        1,
        "never dropped"
    );
    assert_eq!(
        count(&t, &db, "SELECT count(*) FROM shop.customers").await?,
        3
    );

    let permanent = Mode::Entry {
        db,
        ephemeral: false,
    };
    let config = write_config(base.path(), &t, &permanent)?;
    let err = verify_with_store(&config, &store, &request("latest"), false)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("ephemeral"), "{err}");
    Ok(())
}
