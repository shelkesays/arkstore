//! Restore / verify targets and their two-stage resolution (PRD §6.2):
//! first *which* entry (`--target` > env > the source's own name), then
//! per-field overrides (flag > env > entry > engine default).

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use tracing::warn;

use crate::config::name::validate_name;
use crate::config::{Config, Source, SourceType};
use crate::error::{ArkError, Result};
use crate::secrets::Secret;

/// Environment variable naming the target entry.
pub const ENV_TARGET: &str = "ARKSTORE_TARGET";
/// Environment variables for per-field overrides.
pub const ENV_TARGET_HOST: &str = "ARKSTORE_TARGET_HOST";
pub const ENV_TARGET_PORT: &str = "ARKSTORE_TARGET_PORT";
pub const ENV_TARGET_DB: &str = "ARKSTORE_TARGET_DB";
pub const ENV_TARGET_USER: &str = "ARKSTORE_TARGET_USER";
pub const ENV_TARGET_PATH: &str = "ARKSTORE_TARGET_PATH";
/// The target password is never a flag: env, config, or an interactive prompt.
pub const ENV_TARGET_PASSWORD: &str = "ARKSTORE_TARGET_PASSWORD";

/// A named restore/verify target as configured.
#[derive(Debug, Clone, Deserialize)]
pub struct Target {
    pub name: String,
    #[serde(rename = "type")]
    pub target_type: SourceType,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default, alias = "db")]
    pub database: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub password: Option<Secret>,
    /// Mongo authentication database; defaults to `database`.
    #[serde(default)]
    pub auth_db: Option<String>,
    /// File targets: the directory to restore into.
    #[serde(default)]
    pub path: Option<String>,
    /// A throwaway target `verify` may use as-is (never dropped by Arkstore).
    #[serde(default)]
    pub ephemeral: bool,
}

impl Target {
    pub fn validate(&self) -> Result<()> {
        validate_name("target", &self.name)
    }
}

/// Per-field overrides from the command line.
#[derive(Debug, Clone, Default)]
pub struct TargetOverrides {
    pub target: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
    pub path: Option<String>,
}

/// Where environment variables come from — swappable for tests.
pub trait EnvLookup {
    fn get(&self, key: &str) -> Option<String>;
}

/// The real process environment.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessEnv;

impl EnvLookup for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|v| !v.is_empty())
    }
}

impl EnvLookup for HashMap<String, String> {
    fn get(&self, key: &str) -> Option<String> {
        HashMap::get(self, key).cloned()
    }
}

/// A fully resolved target, ready for a loader.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub name: String,
    pub kind: SourceType,
    pub host: Option<String>,
    pub port: u16,
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Option<Secret>,
    pub auth_db: Option<String>,
    pub path: Option<String>,
    pub ephemeral: bool,
}

/// Resolve the target for `source` (PRD §6.2 two-stage precedence).
pub fn resolve_target(
    config: &Config,
    source: &Source,
    overrides: &TargetOverrides,
    env: &dyn EnvLookup,
) -> Result<ResolvedTarget> {
    let entry = select_entry(config, source, overrides, env)?;
    let port = resolve_port(overrides, env, entry)?.unwrap_or(entry.target_type.default_port());
    let pick = |flag: &Option<String>, var: &str, cfg: &Option<String>| -> Option<String> {
        flag.clone()
            .or_else(|| env.get(var))
            .or_else(|| cfg.clone())
    };
    let resolved = ResolvedTarget {
        name: entry.name.clone(),
        kind: entry.target_type,
        host: pick(&overrides.host, ENV_TARGET_HOST, &entry.host),
        port,
        database: pick(&overrides.database, ENV_TARGET_DB, &entry.database),
        user: pick(&overrides.user, ENV_TARGET_USER, &entry.user),
        password: env
            .get(ENV_TARGET_PASSWORD)
            .map(Secret::new)
            .or_else(|| entry.password.clone()),
        auth_db: entry.auth_db.clone(),
        path: pick(&overrides.path, ENV_TARGET_PATH, &entry.path),
        ephemeral: entry.ephemeral,
    };
    require_fields(&resolved)?;
    Ok(resolved)
}

/// Stage one: which `targets` entry — `--target` > env > the source's name.
/// The inline `restore.target` is a fallback **only when no name was given
/// explicitly**: a misspelled `--target` must fail, never silently pick
/// another target.
fn select_entry<'c>(
    config: &'c Config,
    source: &Source,
    overrides: &TargetOverrides,
    env: &dyn EnvLookup,
) -> Result<&'c Target> {
    let explicit = overrides.target.clone().or_else(|| env.get(ENV_TARGET));
    let entry_name = explicit.clone().unwrap_or_else(|| source.name.clone());
    let entry = match config.targets.iter().find(|t| t.name == entry_name) {
        Some(entry) => entry,
        None if explicit.is_some() => {
            return Err(ArkError::Validation(format!(
                "no restore target named `{entry_name}` (selected via --target / {ENV_TARGET})"
            )));
        }
        None => config.restore.target.as_ref().ok_or_else(|| {
            ArkError::Validation(format!(
                "no restore target named `{entry_name}` in `targets`, and no inline `restore.target`"
            ))
        })?,
    };
    if entry.target_type != source.source_type {
        return Err(ArkError::Validation(format!(
            "target `{}` is type `{:?}` but source `{}` is `{:?}`",
            entry.name, entry.target_type, source.name, source.source_type
        )));
    }
    Ok(entry)
}

fn resolve_port(
    overrides: &TargetOverrides,
    env: &dyn EnvLookup,
    entry: &Target,
) -> Result<Option<u16>> {
    if let Some(port) = overrides.port {
        return Ok(Some(port));
    }
    let Some(raw) = env.get(ENV_TARGET_PORT) else {
        return Ok(entry.port);
    };
    raw.parse::<u16>()
        .map(Some)
        .map_err(|_| ArkError::Validation(format!("{ENV_TARGET_PORT}=`{raw}` is not a valid port")))
}

fn require_fields(target: &ResolvedTarget) -> Result<()> {
    let required: &[(&str, bool)] = if target.kind.is_database() {
        &[
            ("host", is_blank(&target.host)),
            ("database", is_blank(&target.database)),
            ("user", is_blank(&target.user)),
        ]
    } else {
        &[("path", is_blank(&target.path))]
    };
    match required.iter().find(|(_, missing)| *missing) {
        Some((field, _)) => Err(ArkError::Validation(format!(
            "target `{}` has no `{field}` after resolution",
            target.name
        ))),
        None => Ok(()),
    }
}

fn is_blank(value: &Option<String>) -> bool {
    value.as_deref().is_none_or(|v| v.trim().is_empty())
}

/// The never-production guard (PRD §6.2): abort when the target *is* the
/// source; warn when it is the same server; reject overlapping file paths.
pub fn check_not_production(source: &Source, target: &ResolvedTarget) -> Result<()> {
    if source.source_type.is_database() {
        let same_server = source.host == target.host && source.port() == target.port;
        let same_db = target.database.as_deref() == Some(source.database());
        if same_server && same_db {
            return Err(ArkError::Refused(format!(
                "target `{}` is the source database itself ({}:{}/{}) — restoring onto the origin is not allowed",
                target.name,
                target.host.as_deref().unwrap_or("?"),
                target.port,
                source.database()
            )));
        }
        if same_server {
            warn!(
                source = %source.name,
                target = %target.name,
                "target is on the same server as the source (different database)"
            );
        }
        return Ok(());
    }
    let (Some(src), Some(dst)) = (source.path.as_deref(), target.path.as_deref()) else {
        return Ok(());
    };
    if paths_overlap(src, dst) {
        return Err(ArkError::Refused(format!(
            "target path `{dst}` overlaps source path `{src}`"
        )));
    }
    Ok(())
}

/// Whether one directory contains (or equals) the other. Both paths are
/// normalised the same way — canonicalised (symlinks resolved) when both
/// exist on this host, otherwise lexically (`.` / `..` collapsed, relative
/// paths anchored at the current directory) — and compared component-wise,
/// so `/srv/data/../data` overlaps `/srv/data` and `/srv/data2` does not.
fn paths_overlap(a: &str, b: &str) -> bool {
    let (a, b) = (Path::new(a), Path::new(b));
    let (a, b) = match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => (a, b),
        _ => (lexical_normalize(a), lexical_normalize(b)),
    };
    a.starts_with(&b) || b.starts_with(&a)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                let _at_root = !out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AwsConfig, RestoreConfig};

    fn src(kind: SourceType) -> Source {
        serde_yaml::from_str(&format!(
            "name: appdb\ntype: {}\nhost: db.internal\nuser: backup\npath: {}\n",
            match kind {
                SourceType::Postgre => "postgre",
                SourceType::File => "file",
                _ => "mysql",
            },
            if kind == SourceType::File {
                "/srv/data"
            } else {
                "null"
            }
        ))
        .unwrap()
    }

    fn cfg(targets: &str, inline: Option<&str>) -> Config {
        let targets: Vec<Target> = serde_yaml::from_str(targets).unwrap();
        Config {
            app: Default::default(),
            aws: AwsConfig {
                bucket: "b".into(),
                region: "r".into(),
                folder: "f".into(),
                endpoint: None,
            },
            cleanup: Default::default(),
            archive: Default::default(),
            concurrency: Default::default(),
            secrets: Default::default(),
            restore: RestoreConfig {
                target: inline.map(|y| serde_yaml::from_str(y).unwrap()),
            },
            verify: Default::default(),
            sources: vec![],
            targets,
        }
    }

    #[test]
    fn picks_entry_by_flag_env_then_source_name() {
        let c = cfg(
            "- {name: appdb, type: postgre, host: stage, db: appdb, user: u}\n\
             - {name: other, type: postgre, host: other, db: o, user: u}\n",
            None,
        );
        let s = src(SourceType::Postgre);
        let none: HashMap<String, String> = HashMap::new();
        let r = resolve_target(&c, &s, &TargetOverrides::default(), &none).unwrap();
        assert_eq!(r.name, "appdb");
        let env: HashMap<String, String> = [(ENV_TARGET.to_string(), "other".to_string())].into();
        assert_eq!(
            resolve_target(&c, &s, &TargetOverrides::default(), &env)
                .unwrap()
                .name,
            "other"
        );
        let o = TargetOverrides {
            target: Some("appdb".into()),
            ..Default::default()
        };
        assert_eq!(resolve_target(&c, &s, &o, &env).unwrap().name, "appdb");
    }

    #[test]
    fn per_field_precedence_flag_env_entry_default() {
        let c = cfg(
            "- {name: appdb, type: postgre, host: stage, db: appdb, user: u}\n",
            None,
        );
        let s = src(SourceType::Postgre);
        let env: HashMap<String, String> = [
            (ENV_TARGET_HOST.to_string(), "envhost".to_string()),
            (ENV_TARGET_PORT.to_string(), "6543".to_string()),
            (ENV_TARGET_PASSWORD.to_string(), "pw".to_string()),
        ]
        .into();
        let o = TargetOverrides {
            host: Some("flaghost".into()),
            ..Default::default()
        };
        let r = resolve_target(&c, &s, &o, &env).unwrap();
        assert_eq!(r.host.as_deref(), Some("flaghost"));
        assert_eq!(r.port, 6543);
        assert_eq!(r.password.as_ref().map(Secret::expose), Some("pw"));
        let none: HashMap<String, String> = HashMap::new();
        let r = resolve_target(&c, &s, &TargetOverrides::default(), &none).unwrap();
        assert_eq!(r.host.as_deref(), Some("stage"));
        assert_eq!(r.port, 5432);
    }

    #[test]
    fn falls_back_to_inline_target_and_rejects_missing_or_mismatched() {
        let c = cfg(
            "[]",
            Some("{name: inline, type: postgre, host: h, db: d, user: u}"),
        );
        let s = src(SourceType::Postgre);
        let none: HashMap<String, String> = HashMap::new();
        assert_eq!(
            resolve_target(&c, &s, &TargetOverrides::default(), &none)
                .unwrap()
                .name,
            "inline"
        );
        let c = cfg("[]", None);
        assert!(resolve_target(&c, &s, &TargetOverrides::default(), &none).is_err());
        let c = cfg(
            "- {name: appdb, type: mysql, host: h, db: d, user: u}\n",
            None,
        );
        assert!(resolve_target(&c, &s, &TargetOverrides::default(), &none).is_err());
        let bad: HashMap<String, String> = [(ENV_TARGET_PORT.to_string(), "x".to_string())].into();
        let c = cfg(
            "- {name: appdb, type: postgre, host: h, db: d, user: u}\n",
            None,
        );
        assert!(resolve_target(&c, &s, &TargetOverrides::default(), &bad).is_err());
    }

    #[test]
    fn explicit_unknown_target_errors_instead_of_falling_back() {
        let c = cfg(
            "[]",
            Some("{name: inline, type: postgre, host: h, db: d, user: u}"),
        );
        let s = src(SourceType::Postgre);
        let o = TargetOverrides {
            target: Some("stagng".into()),
            ..Default::default()
        };
        let none: HashMap<String, String> = HashMap::new();
        let err = resolve_target(&c, &s, &o, &none).unwrap_err().to_string();
        assert!(err.contains("stagng"), "{err}");
        let env: HashMap<String, String> = [(ENV_TARGET.to_string(), "typo".to_string())].into();
        assert!(resolve_target(&c, &s, &TargetOverrides::default(), &env).is_err());
    }

    #[test]
    fn whitespace_only_fields_count_as_missing() {
        let c = cfg(
            "- {name: appdb, type: postgre, host: '   ', db: d, user: u}\n",
            None,
        );
        let s = src(SourceType::Postgre);
        let none: HashMap<String, String> = HashMap::new();
        let err = resolve_target(&c, &s, &TargetOverrides::default(), &none)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`host`"), "{err}");
    }

    #[test]
    fn path_overlap_normalises_dot_dot_and_compares_components() {
        assert!(paths_overlap("/srv/data", "/srv/data/../data"));
        assert!(paths_overlap("/srv/data", "/srv/./data/sub/.."));
        assert!(paths_overlap("/srv/data", "/srv/data/sub"));
        assert!(paths_overlap("/srv", "/srv/data"));
        assert!(!paths_overlap("/srv/data", "/srv/data2"));
        assert!(!paths_overlap("/srv/data", "/srv/data-archive"));
        let tmp = tempfile::tempdir().unwrap();
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let alias = real.join("..").join("real");
        assert!(paths_overlap(
            &real.to_string_lossy(),
            &alias.to_string_lossy()
        ));
    }

    #[test]
    fn never_production_guard() {
        let s = src(SourceType::Postgre);
        let mut t = ResolvedTarget {
            name: "t".into(),
            kind: SourceType::Postgre,
            host: Some("db.internal".into()),
            port: 5432,
            database: Some("appdb".into()),
            user: Some("u".into()),
            password: None,
            auth_db: None,
            path: None,
            ephemeral: false,
        };
        assert!(check_not_production(&s, &t).is_err());
        t.database = Some("appdb_staging".into());
        assert!(check_not_production(&s, &t).is_ok());
        t.host = Some("staging".into());
        t.database = Some("appdb".into());
        assert!(check_not_production(&s, &t).is_ok());

        let f = src(SourceType::File);
        let mut ft = ResolvedTarget {
            kind: SourceType::File,
            host: None,
            database: None,
            path: Some("/srv/data/sub".into()),
            ..t.clone()
        };
        assert!(check_not_production(&f, &ft).is_err());
        ft.path = Some("/srv/data2".into());
        assert!(check_not_production(&f, &ft).is_ok());
        ft.path = Some("/srv".into());
        assert!(check_not_production(&f, &ft).is_err());
    }
}
