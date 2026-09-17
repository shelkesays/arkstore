//! Live PostgreSQL dump test. Runs only when `ARKSTORE_TEST_PG` is set to
//! `host:port:user:password:database` (a throwaway server — the fixture drops
//! and recreates the `shop` schema and a few `public` tables). CI provides a
//! service container; locally, run one and export the variable.

#![cfg(feature = "postgres")]

use std::fs;
use std::path::Path;

use arkstore::config::Config;
use arkstore::error::ArkError;
use arkstore::manifest::{FileRole, Manifest, ObjectEntry, ObjectKind};
use arkstore::ops::backup;
use arkstore::pack::{digest_file, unpack};
use arkstore::store::Store;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// The tests share one database, so they run one at a time.
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

async fn load_fixture(t: &Target) -> Result<(), tokio_postgres::Error> {
    let mut config = tokio_postgres::Config::new();
    config
        .host(&t.host)
        .port(t.port)
        .user(&t.user)
        .password(&t.password)
        .dbname(&t.database);
    let (client, connection) = config.connect(tokio_postgres::NoTls).await?;
    let _driver = tokio::spawn(async move {
        let _ = connection.await;
    });
    client
        .batch_execute(include_str!("fixtures/postgres_fixture.sql"))
        .await
}

fn write_config(base: &Path, t: &Target) -> arkstore::Result<Config> {
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
        "    backup_to_s3: true".to_string(),
        "    delete_after_upload: true".to_string(),
    ];
    fs::write(base.join("arkstore.yaml"), lines.join("\n"))?;
    Config::load(&base.join("arkstore.yaml"))
}

fn object<'a>(manifest: &'a Manifest, name: &str) -> Result<&'a ObjectEntry, String> {
    manifest
        .objects
        .iter()
        .find(|o| o.name == name)
        .ok_or_else(|| format!("object {name} missing from manifest"))
}

fn depends(entry: &ObjectEntry, on: &str) -> bool {
    entry.depends_on.iter().any(|d| d == on)
}

fn text(out: &Path, name: &str) -> Result<String, std::io::Error> {
    fs::read_to_string(out.join(name))
}

/// Back up, download, unpack; return the unpacked directory and manifest.
async fn dump_and_unpack(
    base: &Path,
    t: &Target,
) -> Result<(std::path::PathBuf, Manifest), Box<dyn std::error::Error>> {
    let config = write_config(base, t)?;
    let store = Store::local(&base.join("bucket"))?;
    let failed = backup::run_with_store(&config, Some(&store), None, None, false).await?;
    assert!(failed.is_empty(), "{failed:?}");
    let versioned = store.list("dbbackup/appdb/versioned/").await?;
    assert_eq!(versioned.len(), 1);
    let archive = base.join("backup.tar.gz");
    let key = versioned.first().ok_or("no versioned object")?.key.clone();
    store.download_to_file(&key, &archive).await?;
    let out = base.join("out");
    let report = unpack(&archive, &out)?;
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);
    let manifest = Manifest::from_json(&fs::read(out.join("manifest.json"))?)?;
    Ok((out, manifest))
}

fn check_manifest_header_and_files(out: &Path, manifest: &Manifest) -> TestResult {
    assert_eq!(manifest.snapshot.kind, "pg_snapshot");
    assert!(manifest.snapshot.id.is_some());
    assert!(manifest.consistent);
    assert_eq!(
        manifest.session.get("DateStyle").map(String::as_str),
        Some("ISO, YMD")
    );
    for entry in manifest.objects.iter().flat_map(|o| o.files.iter()) {
        let (size, sha) = digest_file(&out.join(&entry.path))?;
        assert_eq!(
            (size, sha),
            (entry.size, entry.sha256.clone()),
            "{}",
            entry.path
        );
    }
    Ok(())
}

fn check_customers(out: &Path, manifest: &Manifest) -> TestResult {
    let customers = object(manifest, "shop.customers")?;
    assert_eq!(customers.kind, ObjectKind::Table);
    assert_eq!(customers.row_count, Some(3));
    let hash = customers.content_hash.as_deref().unwrap_or_default();
    assert!(hash.starts_with("sum256:"), "{hash}");
    assert!(depends(customers, "shop") && depends(customers, "shop.mood"));
    let structure = text(out, "shop.customers.schema.sql")?;
    assert!(
        structure.contains("CREATE TABLE \"shop\".\"customers\""),
        "{structure}"
    );
    assert!(
        structure.contains("ENABLE ROW LEVEL SECURITY"),
        "{structure}"
    );
    assert!(
        structure.contains("CREATE POLICY \"see_all\""),
        "{structure}"
    );
    assert!(structure.contains("COMMENT ON COLUMN"), "{structure}");
    assert!(
        !structure.contains("GRANT"),
        "privileges are opt-in: {structure}"
    );
    let data = text(out, "shop.customers.data.copy")?;
    assert_eq!(data.lines().count(), 3);
    assert!(
        data.contains("tab\\there") && data.contains("new\\nline"),
        "{data}"
    );
    Ok(())
}

fn check_orders_and_sequences(out: &Path, manifest: &Manifest) -> TestResult {
    let orders = object(manifest, "shop.orders")?;
    assert!(depends(orders, "shop.customers"));
    assert!(
        !depends(orders, "shop.orders"),
        "self-FK is not a dependency"
    );
    assert!(orders.files.iter().any(|f| f.role == FileRole::PostData));
    let post = text(out, "shop.orders.post.sql")?;
    assert!(post.contains("FOREIGN KEY"), "{post}");
    let ddl = text(out, "shop.orders.schema.sql")?;
    assert!(ddl.contains("GENERATED ALWAYS AS IDENTITY"), "{ddl}");
    assert!(ddl.contains("STORED"), "{ddl}");
    assert!(ddl.contains("setval"), "identity value restored: {ddl}");
    assert!(!ddl.contains("FOREIGN KEY"), "{ddl}");
    // Serial sequence is its own object; identity sequence is not.
    object(manifest, "shop.customers_id_seq")?;
    assert!(manifest
        .objects
        .iter()
        .all(|o| o.name != "shop.orders_id_seq"));
    Ok(())
}

fn check_partitions_and_ignores(manifest: &Manifest) -> TestResult {
    // Partitioned parent has structure but no data; partitions carry rows.
    assert_eq!(object(manifest, "shop.events")?.row_count, None);
    let partition = object(manifest, "shop.events_2025")?;
    assert_eq!(partition.row_count, Some(1));
    assert!(depends(partition, "shop.events"));
    // Ignore semantics: data skipped, structure kept; prefix excluded outright.
    let skipped = object(manifest, "public.skip_me")?;
    assert_eq!(skipped.row_count, None);
    assert!(skipped.files.iter().all(|f| f.role != FileRole::Data));
    assert!(manifest
        .objects
        .iter()
        .all(|o| o.name != "public.pg_prefixed_ignore"));
    // An index on an extension's operator class depends on that extension.
    assert!(depends(
        object(manifest, "public.plain")?,
        "extension:pg_trgm"
    ));
    Ok(())
}

fn check_other_kinds_and_plan(out: &Path, manifest: &Manifest) -> TestResult {
    let view = object(manifest, "shop.customer_totals")?;
    assert_eq!(view.kind, ObjectKind::View);
    assert!(depends(view, "shop.total_for(cust integer)"));
    assert_eq!(
        object(manifest, "shop.mood_counts")?.kind,
        ObjectKind::Matview
    );
    let refresh = text(out, "shop.mood_counts.post.sql")?;
    assert!(refresh.contains("REFRESH MATERIALIZED VIEW"), "{refresh}");
    let matview_ddl = text(out, "shop.mood_counts.schema.sql")?;
    assert!(
        !matview_ddl.contains("REFRESH"),
        "refresh must wait for data: {matview_ddl}"
    );
    assert_eq!(
        object(manifest, "shop.orders.orders_touch")?.kind,
        ObjectKind::Trigger
    );
    let trigger = text(out, "shop.orders.orders_touch_disabled.schema.sql")?;
    assert!(trigger.contains("DISABLE TRIGGER"), "{trigger}");
    assert_eq!(object(manifest, "shop.mood")?.kind, ObjectKind::Type);
    assert_eq!(
        object(manifest, "extension:pg_trgm")?.kind,
        ObjectKind::Extension
    );
    assert_eq!(object(manifest, "shop")?.kind, ObjectKind::Schema);
    // The load plan is acyclic apart from the deliberate a_cycle/b_cycle pair.
    let plan = manifest.load_plan();
    let cyclic: Vec<&str> = plan.cyclic.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(cyclic, vec!["shop.a_cycle", "shop.b_cycle"], "{cyclic:?}");
    assert!(plan.layers.len() >= 3, "{}", plan.layers.len());
    Ok(())
}

#[tokio::test]
async fn postgres_dump_round_trips_into_a_manifested_archive() -> TestResult {
    let _serial = SERIAL.lock().await;
    let Some(t) = target() else {
        eprintln!("ARKSTORE_TEST_PG not set; skipping live PostgreSQL test");
        return Ok(());
    };
    load_fixture(&t).await?;
    let base = tempfile::tempdir()?;
    let (out, manifest) = dump_and_unpack(base.path(), &t).await?;
    check_manifest_header_and_files(&out, &manifest)?;
    check_customers(&out, &manifest)?;
    check_orders_and_sequences(&out, &manifest)?;
    check_partitions_and_ignores(&manifest)?;
    check_other_kinds_and_plan(&out, &manifest)
}

#[tokio::test]
async fn postgres_dry_run_reports_without_writing() -> TestResult {
    let _serial = SERIAL.lock().await;
    let Some(t) = target() else {
        return Ok(());
    };
    load_fixture(&t).await?;
    let base = tempfile::tempdir()?;
    let config = write_config(base.path(), &t)?;
    let failed = backup::run_with_store(&config, None, None, None, true).await?;
    assert!(failed.is_empty(), "{failed:?}");
    assert!(!base.path().join("local").exists());
    Ok(())
}

#[tokio::test]
async fn postgres_binary_copy_is_refused_for_now() -> TestResult {
    let _serial = SERIAL.lock().await;
    let Some(t) = target() else {
        return Ok(());
    };
    let base = tempfile::tempdir()?;
    let mut config = write_config(base.path(), &t)?;
    config.sources[0].copy_format = Some(arkstore::config::CopyFormat::Binary);
    let store = Store::local(&base.path().join("bucket"))?;
    let failed = backup::run_with_store(&config, Some(&store), None, None, false).await?;
    assert_eq!(failed, vec!["appdb".to_string()]);
    let ctx = arkstore::engine::DumpContext {
        work_dir: base.path(),
        stamp: "2026-01-01-000000",
        timezone: "UTC",
    };
    let outcome = arkstore::engine::dump_database(&config.sources[0], &ctx).await;
    assert!(
        matches!(outcome, Err(ArkError::NotImplemented(_))),
        "{outcome:?}"
    );
    Ok(())
}
