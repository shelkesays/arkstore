//! Live PostgreSQL dump test. Runs only when `ARKSTORE_TEST_PG` is set to
//! `host:port:user:password:database` (a throwaway server — the fixture drops
//! and recreates the `shop` schema and a few `public` tables). CI provides a
//! service container; locally, run one and export the variable.

#![cfg(feature = "postgres")]

use std::fs;
use std::path::Path;

use arkstore::config::Config;
use arkstore::error::ArkError;
use arkstore::manifest::{FileRole, Manifest, ObjectKind};
use arkstore::ops::backup;
use arkstore::pack::unpack;
use arkstore::store::Store;

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
    tokio::spawn(async move {
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

fn object<'a>(manifest: &'a Manifest, name: &str) -> &'a arkstore::manifest::ObjectEntry {
    manifest
        .objects
        .iter()
        .find(|o| o.name == name)
        .unwrap_or_else(|| panic!("object {name} missing from manifest"))
}

#[tokio::test]
async fn postgres_dump_round_trips_into_a_manifested_archive() {
    let _serial = SERIAL.lock().await;
    let Some(t) = target() else {
        eprintln!("ARKSTORE_TEST_PG not set; skipping live PostgreSQL test");
        return;
    };
    load_fixture(&t).await.unwrap();
    let base = tempfile::tempdir().unwrap();
    let config = write_config(base.path(), &t).unwrap();
    let store = Store::local(&base.path().join("bucket")).unwrap();

    let failed = backup::run_with_store(&config, Some(&store), None, None, false)
        .await
        .unwrap();
    assert!(failed.is_empty(), "{failed:?}");

    let versioned = store.list("dbbackup/appdb/versioned/").await.unwrap();
    assert_eq!(versioned.len(), 1);
    let archive = base.path().join("backup.tar.gz");
    store
        .download_to_file(&versioned[0].key, &archive)
        .await
        .unwrap();
    let out = base.path().join("out");
    let report = unpack(&archive, &out).unwrap();
    assert!(report.skipped.is_empty(), "{:?}", report.skipped);

    let manifest = Manifest::from_json(&fs::read(out.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest.snapshot.kind, "pg_snapshot");
    assert!(manifest.snapshot.id.is_some());
    assert!(manifest.consistent);
    assert_eq!(
        manifest.session.get("DateStyle").map(String::as_str),
        Some("ISO, YMD")
    );

    // Every manifest file exists with the recorded size and digest.
    for file in manifest.file_paths() {
        let entry = manifest
            .objects
            .iter()
            .flat_map(|o| o.files.iter())
            .find(|f| f.path == file)
            .unwrap();
        let (size, sha) = arkstore::pack::digest_file(&out.join(file)).unwrap();
        assert_eq!((size, sha), (entry.size, entry.sha256.clone()), "{file}");
    }

    let customers = object(&manifest, "shop.customers");
    assert_eq!(customers.kind, ObjectKind::Table);
    assert_eq!(customers.row_count, Some(3));
    assert!(customers
        .content_hash
        .as_deref()
        .unwrap()
        .starts_with("sum256:"));
    assert!(customers.depends_on.contains(&"shop".to_string()));
    assert!(customers.depends_on.contains(&"shop.mood".to_string()));
    let structure = fs::read_to_string(out.join("shop.customers.schema.sql")).unwrap();
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
    let data = fs::read_to_string(out.join("shop.customers.data.copy")).unwrap();
    assert_eq!(data.lines().count(), 3);
    assert!(
        data.contains("tab\\there") && data.contains("new\\nline"),
        "{data}"
    );

    let orders = object(&manifest, "shop.orders");
    assert!(orders.depends_on.contains(&"shop.customers".to_string()));
    assert!(
        !orders.depends_on.contains(&"shop.orders".to_string()),
        "self-FK is not a dependency"
    );
    assert!(orders.files.iter().any(|f| f.role == FileRole::PostData));
    let fks = fs::read_to_string(out.join("shop.orders.post.sql")).unwrap();
    assert!(fks.contains("FOREIGN KEY"), "{fks}");
    let orders_ddl = fs::read_to_string(out.join("shop.orders.schema.sql")).unwrap();
    assert!(
        orders_ddl.contains("GENERATED ALWAYS AS IDENTITY"),
        "{orders_ddl}"
    );
    assert!(orders_ddl.contains("STORED"), "{orders_ddl}");
    assert!(
        orders_ddl.contains("setval"),
        "identity value restored: {orders_ddl}"
    );
    assert!(!orders_ddl.contains("FOREIGN KEY"), "{orders_ddl}");

    // Serial sequence is its own object; identity sequence is not.
    object(&manifest, "shop.customers_id_seq");
    assert!(manifest
        .objects
        .iter()
        .all(|o| o.name != "shop.orders_id_seq"));

    // Partitioned parent has structure but no data; partitions carry rows.
    let events = object(&manifest, "shop.events");
    assert_eq!(events.row_count, None);
    assert_eq!(object(&manifest, "shop.events_2025").row_count, Some(1));
    assert!(object(&manifest, "shop.events_2025")
        .depends_on
        .contains(&"shop.events".to_string()));

    // Ignore semantics: data skipped, structure kept; prefix excluded outright.
    let skipped = object(&manifest, "public.skip_me");
    assert_eq!(skipped.row_count, None);
    assert!(skipped.files.iter().all(|f| f.role != FileRole::Data));
    assert!(manifest
        .objects
        .iter()
        .all(|o| o.name != "public.pg_prefixed_ignore"));

    // Views, matviews, functions, triggers, types, extensions, schemas.
    assert_eq!(
        object(&manifest, "shop.customer_totals").kind,
        ObjectKind::View
    );
    assert!(object(&manifest, "shop.customer_totals")
        .depends_on
        .contains(&"shop.total_for(cust integer)".to_string()));
    assert_eq!(
        object(&manifest, "shop.mood_counts").kind,
        ObjectKind::Matview
    );
    let refresh = fs::read_to_string(out.join("shop.mood_counts.post.sql")).unwrap();
    assert!(refresh.contains("REFRESH MATERIALIZED VIEW"), "{refresh}");
    let matview_ddl = fs::read_to_string(out.join("shop.mood_counts.schema.sql")).unwrap();
    assert!(
        !matview_ddl.contains("REFRESH"),
        "refresh must wait for data: {matview_ddl}"
    );
    assert_eq!(
        object(&manifest, "shop.orders.orders_touch").kind,
        ObjectKind::Trigger
    );
    let trigger =
        fs::read_to_string(out.join("shop.orders.orders_touch_disabled.schema.sql")).unwrap();
    assert!(trigger.contains("DISABLE TRIGGER"), "{trigger}");
    assert_eq!(object(&manifest, "shop.mood").kind, ObjectKind::Type);
    assert_eq!(
        object(&manifest, "extension:pg_trgm").kind,
        ObjectKind::Extension
    );
    assert_eq!(object(&manifest, "shop").kind, ObjectKind::Schema);

    // The load plan is acyclic apart from the deliberate a_cycle/b_cycle pair.
    let plan = manifest.load_plan();
    let cyclic: Vec<&str> = plan.cyclic.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(cyclic, vec!["shop.a_cycle", "shop.b_cycle"], "{cyclic:?}");
    assert!(plan.layers.len() >= 3, "{}", plan.layers.len());
}

#[tokio::test]
async fn postgres_dry_run_reports_without_writing() {
    let _serial = SERIAL.lock().await;
    let Some(t) = target() else {
        return;
    };
    load_fixture(&t).await.unwrap();
    let base = tempfile::tempdir().unwrap();
    let config = write_config(base.path(), &t).unwrap();
    let failed = backup::run_with_store(&config, None, None, None, true)
        .await
        .unwrap();
    assert!(failed.is_empty(), "{failed:?}");
    assert!(!base.path().join("local").exists());
}

#[tokio::test]
async fn postgres_binary_copy_is_refused_for_now() {
    let _serial = SERIAL.lock().await;
    let Some(t) = target() else {
        return;
    };
    let base = tempfile::tempdir().unwrap();
    let mut config = write_config(base.path(), &t).unwrap();
    config.sources[0].copy_format = Some(arkstore::config::CopyFormat::Binary);
    let store = Store::local(&base.path().join("bucket")).unwrap();
    let failed = backup::run_with_store(&config, Some(&store), None, None, false)
        .await
        .unwrap();
    assert_eq!(failed, vec!["appdb".to_string()]);
    let err = arkstore::engine::dump_database(
        &config.sources[0],
        &arkstore::engine::DumpContext {
            work_dir: base.path(),
            stamp: "2026-01-01-000000",
            timezone: "UTC",
        },
    )
    .await
    .unwrap_err();
    assert!(matches!(err, ArkError::NotImplemented(_)), "{err}");
}
