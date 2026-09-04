//! End-to-end: back up a file tree to a local object store, list it, restore
//! it into an empty target, and exercise the local lifecycle. Helpers return
//! `Result`; only `#[test]` bodies unwrap.

use std::fs;
use std::path::Path;

use arkstore::cli::RestoreAction;
use arkstore::config::{Config, TargetOverrides};
use arkstore::error::ArkError;
use arkstore::layout::{latest_key, parse_key, BackupKind};
use arkstore::ops::{backup, restore, RestoreRequest};
use arkstore::store::Store;

fn write_config_for(base: &Path, extra_source: &str) -> arkstore::Result<Config> {
    let yaml = format!(
        "app:\n  timezone: UTC\n  local_dir: {local}\naws:\n  bucket: b\n  region: r\n  folder: dbbackup\n\
         sources:\n  - name: files\n    type: file\n    path: {src}\n    ignore: ['*.tmp']\n{extra_source}\
         targets:\n  - name: files\n    type: file\n    path: {dst}\n",
        local = base.join("local").display(),
        src = base.join("src").display(),
        dst = base.join("dst").display(),
    );
    fs::write(base.join("arkstore.yaml"), yaml)?;
    Config::load(&base.join("arkstore.yaml"))
}

fn write_tree(root: &Path) -> std::io::Result<()> {
    fs::create_dir_all(root.join("sub"))?;
    fs::write(root.join("a.txt"), "alpha")?;
    fs::write(root.join("sub/b.txt"), "beta")?;
    fs::write(root.join("scratch.tmp"), "junk")?;
    fs::write(root.join(".DS_Store"), "junk")
}

/// Tree + config + local store + one completed backup. Returns the stamp.
async fn seed(base: &Path) -> arkstore::Result<(Config, Store, String)> {
    write_tree(&base.join("src"))?;
    let config = write_config_for(base, "")?;
    let store = Store::local(&base.join("bucket"))?;
    let failed = backup::run_with_store(&config, Some(&store), None, None, false).await?;
    if !failed.is_empty() {
        return Err(ArkError::Internal(format!("backup failed: {failed:?}")));
    }
    let versioned = store.list("dbbackup/files/versioned/").await?;
    let parsed = versioned
        .first()
        .and_then(|o| parse_key("dbbackup", &o.key))
        .ok_or_else(|| ArkError::Internal("no versioned backup".into()))?;
    match parsed.kind {
        BackupKind::Versioned { stamp } => Ok((config, store, stamp)),
        BackupKind::Latest => Err(ArkError::Internal("latest listed as versioned".into())),
    }
}

fn restore_request(from: &str, target_path: Option<&Path>) -> RestoreRequest {
    RestoreRequest {
        source: "files".into(),
        action: RestoreAction::Restore,
        from: from.into(),
        target: TargetOverrides {
            path: target_path.map(|p| p.to_string_lossy().into_owned()),
            ..Default::default()
        },
    }
}

#[tokio::test]
async fn backup_uploads_versioned_and_latest_and_lists_them() {
    let base = tempfile::tempdir().unwrap();
    let (config, store, _stamp) = seed(base.path()).await.unwrap();

    let versioned = store.list("dbbackup/files/versioned/").await.unwrap();
    assert_eq!(versioned.len(), 1);
    let latest = store.head(&latest_key("dbbackup", "files")).await.unwrap();
    assert_eq!(
        latest.size, versioned[0].size,
        "latest pointer mirrors the versioned object"
    );
    assert!(
        !base.path().join("local").exists(),
        "delete_after_upload leaves no local copy"
    );

    let listed = restore::list_backups(&config, &store, config.source("files").unwrap())
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
}

#[tokio::test]
async fn restore_latest_applies_ignores_refuses_non_empty_and_dry_runs() {
    let base = tempfile::tempdir().unwrap();
    let (config, store, _stamp) = seed(base.path()).await.unwrap();
    let request = restore_request("latest", None);

    let failed = restore::run_with_store(&config, &store, &request, false)
        .await
        .unwrap();
    assert!(failed.is_empty());
    let dst = base.path().join("dst");
    assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "alpha");
    assert_eq!(fs::read_to_string(dst.join("sub/b.txt")).unwrap(), "beta");
    assert!(!dst.join("scratch.tmp").exists(), "ignore pattern applied");
    assert!(
        !dst.join(".DS_Store").exists(),
        "default ignore_extensions applied"
    );

    let err = restore::run_with_store(&config, &store, &request, false)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("not empty"), "{err}");

    let dry = restore_request("latest", Some(&base.path().join("dry")));
    restore::run_with_store(&config, &store, &dry, true)
        .await
        .unwrap();
    assert!(!base.path().join("dry").exists(), "dry run writes nothing");
}

#[tokio::test]
async fn restore_by_stamp_works_and_foreign_keys_are_refused() {
    let base = tempfile::tempdir().unwrap();
    let (config, store, stamp) = seed(base.path()).await.unwrap();

    let by_stamp = restore_request(&stamp, Some(&base.path().join("dst2")));
    restore::run_with_store(&config, &store, &by_stamp, false)
        .await
        .unwrap();
    assert!(base.path().join("dst2/a.txt").is_file());

    let foreign = restore_request(
        "dbbackup/other/other.latest.tar.gz",
        Some(&base.path().join("dst3")),
    );
    let err = restore::run_with_store(&config, &store, &foreign, false)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("refusing"), "{err}");
    assert!(!base.path().join("dst3").exists());
}

#[tokio::test]
async fn local_only_source_keeps_copies_and_prunes_to_local_retention() {
    let base = tempfile::tempdir().unwrap();
    write_tree(&base.path().join("src")).unwrap();
    let extra = "  - name: local-only\n    type: file\n    path: SRC\n    backup_to_s3: false\n    local_retention: 2\n"
        .replace("SRC", &base.path().join("src").display().to_string());
    let config = write_config_for(base.path(), &extra).unwrap();

    for _ in 0..3 {
        let failed = backup::run_with_store(&config, None, None, Some("local-only"), false)
            .await
            .unwrap();
        assert!(failed.is_empty(), "{failed:?}");
        // Stamps have second resolution; make successive runs distinct.
        std::thread::sleep(std::time::Duration::from_millis(1100));
    }
    let versioned_dir = base.path().join("local/local-only/versioned");
    assert_eq!(
        fs::read_dir(&versioned_dir).unwrap().count(),
        2,
        "pruned to local_retention"
    );
    assert!(base
        .path()
        .join("local/local-only/local-only.latest.tar.gz")
        .is_file());
}

#[tokio::test]
async fn database_sources_report_missing_backend_without_aborting_the_run() {
    let base = tempfile::tempdir().unwrap();
    write_tree(&base.path().join("src")).unwrap();
    let extra =
        "  - name: appdb\n    type: postgre\n    host: h\n    user: u\n    backup_to_s3: false\n";
    let config = write_config_for(base.path(), extra).unwrap();
    let store = Store::local(&base.path().join("bucket")).unwrap();
    let failed = backup::run_with_store(&config, Some(&store), None, None, false)
        .await
        .unwrap();
    assert_eq!(
        failed,
        vec!["appdb".to_string()],
        "file source succeeded, db source isolated"
    );
    assert_eq!(
        store.list("dbbackup/files/versioned/").await.unwrap().len(),
        1
    );
}
