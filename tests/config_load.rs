//! Integration tests: loading the three-file configuration, sibling
//! `sources.yaml` / `targets.yaml`, secrets hydration, and validation errors
//! that name the offending field. Helpers return `Result` so only `#[test]`
//! bodies unwrap.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use arkstore::config::{Config, CopyFormat, SourceType};
use arkstore::secrets::{load_secrets, ENV_SECRETS_FILE};
use tempfile::TempDir;

const BASE: &str = "\
app:
  timezone: Europe/Berlin
aws:
  bucket: backups
  region: eu-central-1
";

const SOURCES: &str = "sources:
  - name: appdb
    type: postgre
    host: db.internal
    user: backup
  - name: docs
    type: mongo
    host: mongo.internal
    user: backup
  - name: files
    type: file
    path: /srv/files
";

const TARGETS: &str = "targets:
  - name: appdb
    type: postgre
    host: staging.internal
    db: appdb
    user: restore
    ephemeral: true
";

fn write_file(dir: &Path, name: &str, text: &str) -> std::io::Result<()> {
    fs::write(dir.join(name), text)
}

fn load_from(files: &[(&str, &str)]) -> arkstore::Result<(TempDir, Config)> {
    let dir = tempfile::tempdir()?;
    for (name, text) in files {
        write_file(dir.path(), name, text)?;
    }
    let config = Config::load(&dir.path().join("arkstore.yaml"))?;
    Ok((dir, config))
}

#[test]
fn loads_sibling_files_and_postgres_defaults() {
    let (_dir, config) = load_from(&[
        ("arkstore.yaml", BASE),
        ("sources.yaml", SOURCES),
        ("targets.yaml", TARGETS),
    ])
    .unwrap();
    assert_eq!(config.sources.len(), 3);
    assert_eq!(config.targets.len(), 1);
    assert!(config.targets[0].ephemeral);
    assert_eq!(config.timezone().unwrap().name(), "Europe/Berlin");

    let pg = config.source("appdb").unwrap();
    assert!(pg.effective_structure());
    assert!(pg.data);
    assert_eq!(pg.port(), 5432);
    assert_eq!(pg.database(), "appdb");
    assert_eq!(pg.effective_ignore_startswith(), ["pg_", "rds_", "awsdms_"]);
    assert_eq!(pg.copy_format, CopyFormat::Text);
    assert!(!pg.include_privileges);
    assert!(pg.backup_to_s3 && pg.delete_after_upload);
    assert_eq!(pg.local_retention, 0);
}

#[test]
fn applies_mongo_and_file_defaults_and_selection() {
    let (_dir, config) = load_from(&[("arkstore.yaml", BASE), ("sources.yaml", SOURCES)]).unwrap();

    let mongo = config.source("docs").unwrap();
    assert!(
        !mongo.effective_structure(),
        "mongo has no structure by default"
    );
    assert_eq!(mongo.port(), 27017);
    assert_eq!(mongo.auth_database(), "docs");
    assert_eq!(
        mongo.effective_ignore(),
        ["system.profile", "local.startup_log"]
    );

    let files = config.source("files").unwrap();
    assert_eq!(
        files.effective_ignore_extensions(),
        ["photoslibrary", "DS_Store", "localized"]
    );

    assert_eq!(
        config
            .selected_sources(Some(SourceType::Postgre), None)
            .len(),
        1
    );
    assert_eq!(config.selected_sources(None, Some("docs")).len(), 1);
    assert_eq!(config.selected_sources(None, None).len(), 3);
    assert!(config.source("nope").is_err());
}

#[test]
fn inline_sources_take_precedence_over_sibling_file() {
    let inline = format!("{BASE}sources:\n  - {{name: inline, type: file, path: /x}}\n");
    let (_dir, config) = load_from(&[
        ("arkstore.yaml", &inline),
        (
            "sources.yaml",
            "sources:\n  - {name: sib, type: file, path: /y}\n",
        ),
    ])
    .unwrap();
    assert_eq!(config.sources.len(), 1);
    assert_eq!(config.sources[0].name, "inline");
}

#[test]
fn validation_errors_name_the_field() {
    let bad_tz = BASE.replace("Europe/Berlin", "Mars/Olympus");
    let cases: Vec<(String, &str)> = vec![
        (format!("{BASE}sources:\n  - {{name: 'bad name', type: file, path: /x}}\n"), "bad name"),
        (format!("{BASE}sources:\n  - {{name: a, type: postgre, user: u}}\n"), "`host`"),
        (format!("{BASE}sources:\n  - {{name: b, type: mysql, host: h, user: u, copy_format: binary}}\n"), "copy_format"),
        (format!("{BASE}sources:\n  - {{name: a, type: postgre, host: h, user: u, allow_unsnapshotted_tables: true}}\n"), "allow_unsnapshotted_tables"),
        (format!("{BASE}sources:\n  - {{name: a, type: file, path: /x, archive: [{{table: t, time_column: c}}]}}\n"), "`archive`"),
        (format!("{BASE}sources:\n  - {{name: a, type: file, path: /x}}\n  - {{name: a, type: file, path: /y}}\n"), "duplicate source"),
        (format!("{BASE}targets:\n  - {{name: 'x/y', type: postgre}}\n"), "x/y"),
        (bad_tz, "timezone"),
    ];
    for (yaml, needle) in cases {
        let err = load_from(&[("arkstore.yaml", &yaml)])
            .unwrap_err()
            .to_string();
        assert!(err.contains(needle), "expected `{needle}` in `{err}`");
    }
}

#[test]
fn secrets_file_hydrates_sources_and_targets_by_name() {
    let yaml = format!(
        "{BASE}sources:\n  - {{name: appdb, type: postgre, host: placeholder, user: u}}\n\
         targets:\n  - {{name: staging, type: postgre, host: s, db: d, user: u}}\n"
    );
    let secrets = "sources:
  appdb: {password: src-pw, host: db.internal}
  ghost: {password: unused}
targets:
  staging: {password: tgt-pw}
";
    let (dir, mut config) =
        load_from(&[("arkstore.yaml", &yaml), ("secrets.yaml", secrets)]).unwrap();
    let env: HashMap<String, String> = [(
        ENV_SECRETS_FILE.to_string(),
        dir.path()
            .join("secrets.yaml")
            .to_string_lossy()
            .into_owned(),
    )]
    .into();
    load_secrets(&mut config, &env).unwrap();

    let src = config.source("appdb").unwrap();
    assert_eq!(src.password.as_ref().unwrap().expose(), "src-pw");
    assert_eq!(
        src.host.as_deref(),
        Some("db.internal"),
        "secret may override host"
    );
    assert_eq!(
        config.targets[0].password.as_ref().unwrap().expose(),
        "tgt-pw"
    );
    assert!(
        !format!("{config:?}").contains("src-pw"),
        "Debug must not leak secrets"
    );
}
