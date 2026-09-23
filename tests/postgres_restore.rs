//! Live PostgreSQL restore test: dump the fixture, restore it into a fresh
//! database on the same server, and check what landed. Runs only when
//! `ARKSTORE_TEST_PG` is set to `host:port:user:password:database` (a
//! throwaway server — the test drops and recreates `<database>_restore` and
//! `<database>_restore2`).

#![cfg(feature = "postgres")]

use std::fs;
use std::path::Path;

use arkstore::cli::RestoreAction;
use arkstore::config::{Config, TargetOverrides};
use arkstore::ops::{backup, restore, RestoreRequest};
use arkstore::pack::{digest_file, pack_dir, unpack};
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

async fn load_fixture(t: &Target) -> Result<(), tokio_postgres::Error> {
    client(t, &t.database)
        .await?
        .batch_execute(include_str!("fixtures/postgres_fixture.sql"))
        .await
}

/// Drop (forcibly) and recreate `name`.
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

async fn count(t: &Target, database: &str, sql: &str) -> Result<i64, tokio_postgres::Error> {
    let row = client(t, database).await?.query_one(sql, &[]).await?;
    row.try_get(0)
}

fn write_config(base: &Path, t: &Target, target_db: &str) -> arkstore::Result<Config> {
    let lines = [
        "app:".to_string(),
        "  timezone: UTC".to_string(),
        format!("  local_dir: {}", base.join("local").display()),
        "aws:".to_string(),
        "  bucket: b".to_string(),
        "  region: r".to_string(),
        "  folder: dbbackup".to_string(),
        "sources:".to_string(),
        "  - name: appdb".to_string(),
        "    type: postgre".to_string(),
        format!("    host: {}", t.host),
        format!("    port: {}", t.port),
        format!("    user: {}", t.user),
        format!("    password: {}", t.password),
        format!("    database: {}", t.database),
        "    tls: disable".to_string(),
        "    ignore: ['public.skip_me']".to_string(),
        "targets:".to_string(),
        "  - name: appdb".to_string(),
        "    type: postgre".to_string(),
        format!("    host: {}", t.host),
        format!("    port: {}", t.port),
        format!("    user: {}", t.user),
        format!("    password: {}", t.password),
        format!("    db: {target_db}"),
        "    tls: disable".to_string(),
    ];
    fs::write(base.join("arkstore.yaml"), lines.join("\n"))?;
    Config::load(&base.join("arkstore.yaml"))
}

fn request(from: &str) -> RestoreRequest {
    RestoreRequest {
        source: "appdb".into(),
        action: RestoreAction::Restore,
        from: from.into(),
        target: TargetOverrides::default(),
    }
}

/// Fixture into the source, one backup into a local store.
async fn seed(
    base: &Path,
    t: &Target,
    target_db: &str,
) -> Result<(Config, Store), Box<dyn std::error::Error>> {
    load_fixture(t).await?;
    let config = write_config(base, t, target_db)?;
    let store = Store::local(&base.join("bucket"))?;
    let failed = backup::run_with_store(&config, Some(&store), None, None, false).await?;
    assert!(failed.is_empty(), "{failed:?}");
    Ok((config, store))
}

async fn check_restored_database(t: &Target, db: &str) -> TestResult {
    let checks: [(&str, i64); 9] = [
        ("SELECT count(*) FROM shop.customers", 3),
        ("SELECT count(*) FROM shop.orders", 3),
        ("SELECT count(*) FROM shop.events", 2),
        ("SELECT count(*) FROM ONLY shop.events_2025", 1),
        ("SELECT count(*) FROM shop.mood_counts", 3),
        ("SELECT count(*) FROM shop.\"Mixed Case\"", 1),
        ("SELECT count(*) FROM public.skip_me", 0),
        ("SELECT count(*) FROM pg_constraint WHERE contype = 'f' AND connamespace = 'shop'::regnamespace", 4),
        ("SELECT count(*) FROM pg_trigger WHERE tgname = 'orders_touch_disabled' AND tgenabled = 'D'", 1),
    ];
    for (sql, expected) in checks {
        assert_eq!(count(t, db, sql).await?, expected, "{sql}");
    }
    assert_eq!(
        count(t, db, "SELECT last_value FROM shop.order_number_seq").await?,
        1045
    );
    assert_eq!(
        count(
            t,
            db,
            "SELECT count(*) FROM pg_class WHERE relname = 'customers' AND relrowsecurity"
        )
        .await?,
        1
    );
    assert_eq!(count(t, db, "SELECT shop.total_for(1)::bigint").await?, 112);
    // Triggers are created after the data: placed_at is exactly the source's,
    // not rewritten by the BEFORE INSERT trigger during the load.
    let sql = "SELECT string_agg(placed_at::text, ',' ORDER BY id) FROM shop.orders";
    assert_eq!(
        text_value(t, db, sql).await?,
        text_value(t, &t.database, sql).await?
    );
    Ok(())
}

async fn text_value(
    t: &Target,
    database: &str,
    sql: &str,
) -> Result<String, tokio_postgres::Error> {
    let row = client(t, database).await?.query_one(sql, &[]).await?;
    row.try_get(0)
}

#[tokio::test]
async fn restore_recreates_the_database_from_the_manifest() -> TestResult {
    let _serial = SERIAL.lock().await;
    let Some(t) = target() else {
        eprintln!("ARKSTORE_TEST_PG not set; skipping live PostgreSQL test");
        return Ok(());
    };
    let db = format!("{}_restore", t.database);
    let base = tempfile::tempdir()?;
    let (config, store) = seed(base.path(), &t, &db).await?;
    fresh_database(&t, &db).await?;

    // Dry run touches nothing.
    let failed = restore::run_with_store(&config, &store, &request("latest"), true).await?;
    assert!(failed.is_empty(), "{failed:?}");
    assert_eq!(
        count(
            &t,
            &db,
            "SELECT count(*) FROM pg_namespace WHERE nspname = 'shop'"
        )
        .await?,
        0
    );

    let failed = restore::run_with_store(&config, &store, &request("latest"), false).await?;
    assert!(failed.is_empty(), "{failed:?}");
    check_restored_database(&t, &db).await?;

    // A second restore into the now non-empty target is refused before any transfer.
    let err = restore::run_with_store(&config, &store, &request("latest"), false)
        .await
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default();
    assert!(err.contains("is not empty"), "{err}");
    Ok(())
}

/// Take the stored archive apart, add a row to one data file, and rewrite the
/// manifest's size and digest for it so the file passes the integrity check
/// and the mismatch surfaces at load time as a row-count difference.
async fn tamper_archive(
    base: &Path,
    store: &Store,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let work = unpack_stored_archive(base, store).await?;
    let data = work.join("shop.scratch.data.copy");
    let mut scratch = fs::read_to_string(&data)?;
    scratch.push_str("zz\t9\n");
    fs::write(&data, scratch)?;
    patch_manifest_digest(&work, "shop.scratch.data.copy")?;
    let tampered = base.join("tampered.tar.gz");
    pack_dir(&work, &tampered)?;
    Ok(tampered)
}

async fn unpack_stored_archive(
    base: &Path,
    store: &Store,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    let versioned = store.list("dbbackup/appdb/versioned/").await?;
    let key = versioned.first().ok_or("no versioned backup")?.key.clone();
    let original = base.join("original.tar.gz");
    store.download_to_file(&key, &original).await?;
    let work = base.join("work");
    unpack(&original, &work)?;
    Ok(work)
}

/// Rewrite `size` / `sha256` of one file entry to the file's current bytes.
fn patch_manifest_digest(work: &Path, file_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let (size, sha256) = digest_file(&work.join(file_path))?;
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(work.join("manifest.json"))?)?;
    let objects = manifest["objects"].as_array_mut().ok_or("objects")?;
    let files = objects
        .iter_mut()
        .filter_map(|o| o["files"].as_array_mut())
        .flatten()
        .filter(|f| f["path"] == file_path);
    for file in files {
        file["size"] = serde_json::json!(size);
        file["sha256"] = serde_json::json!(sha256);
    }
    fs::write(
        work.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(())
}

#[tokio::test]
async fn a_corrupt_data_file_fails_only_its_object() -> TestResult {
    let _serial = SERIAL.lock().await;
    let Some(t) = target() else {
        return Ok(());
    };
    let db = format!("{}_restore2", t.database);
    let base = tempfile::tempdir()?;
    let (config, store) = seed(base.path(), &t, &db).await?;
    fresh_database(&t, &db).await?;

    let tampered = tamper_archive(base.path(), &store).await?;
    let from = tampered.to_string_lossy().into_owned();
    let failed = restore::run_with_store(&config, &store, &request(&from), false).await?;
    assert_eq!(
        failed,
        vec!["appdb".to_string()],
        "one failed object exits 1"
    );
    check_restored_database(&t, &db).await?;
    // The structure was applied, the COPY was rolled back on the count
    // mismatch, so the table exists and holds nothing.
    assert_eq!(
        count(
            &t,
            &db,
            "SELECT count(*) FROM pg_class WHERE relname = 'scratch'"
        )
        .await?,
        1
    );
    assert_eq!(
        count(&t, &db, "SELECT count(*) FROM shop.scratch").await?,
        0,
        "rows of a failed object are rolled back"
    );
    Ok(())
}
