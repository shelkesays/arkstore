//! Object-key layout (KB §1) and the sortable backup stamp.
//!
//! ```text
//! <folder>/<source>/versioned/<source>.<stamp>.tar.gz   # immutable
//! <folder>/<source>/<source>.latest.tar.gz              # the only mutable object
//! ```
//!
//! Writer and reader share one stamp format, so a key round-trips exactly;
//! anything that does not parse is *not* a backup and cleanup leaves it alone.

use chrono::{DateTime, NaiveDateTime, Utc};
use chrono_tz::Tz;

use crate::config::name::validate_name;

/// Lexicographically sortable, second-resolution stamp rendered in `app.timezone`.
pub const STAMP_FORMAT: &str = "%Y-%m-%d-%H%M%S";
/// Archive file suffix.
pub const ARCHIVE_SUFFIX: &str = ".tar.gz";

/// Render `now` as a backup stamp in `tz`.
pub fn render_stamp(now: DateTime<Utc>, tz: Tz) -> String {
    now.with_timezone(&tz).format(STAMP_FORMAT).to_string()
}

/// Parse a stamp produced by [`render_stamp`] (wall-clock in `app.timezone`).
pub fn parse_stamp(stamp: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(stamp, STAMP_FORMAT).ok()
}

fn folder_prefix(folder: &str) -> String {
    let trimmed = folder.trim_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}/")
    }
}

/// `<folder>/<source>/`
pub fn source_prefix(folder: &str, source: &str) -> String {
    format!("{}{source}/", folder_prefix(folder))
}

/// `<folder>/<source>/versioned/`
pub fn versioned_prefix(folder: &str, source: &str) -> String {
    format!("{}versioned/", source_prefix(folder, source))
}

/// `<source>.<stamp>.tar.gz`
pub fn versioned_file_name(source: &str, stamp: &str) -> String {
    format!("{source}.{stamp}{ARCHIVE_SUFFIX}")
}

/// `<source>.latest.tar.gz`
pub fn latest_file_name(source: &str) -> String {
    format!("{source}.latest{ARCHIVE_SUFFIX}")
}

/// Full key of one versioned backup.
pub fn versioned_key(folder: &str, source: &str, stamp: &str) -> String {
    format!(
        "{}{}",
        versioned_prefix(folder, source),
        versioned_file_name(source, stamp)
    )
}

/// Full key of the `latest` pointer.
pub fn latest_key(folder: &str, source: &str) -> String {
    format!(
        "{}{}",
        source_prefix(folder, source),
        latest_file_name(source)
    )
}

/// What a backup key denotes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupKind {
    Versioned { stamp: String },
    Latest,
}

/// A key that matched the backup layout exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedKey {
    pub source: String,
    pub kind: BackupKind,
}

/// Parse `key` under `folder`. `None` means "not a backup" — the caller must
/// treat such keys as untouchable.
pub fn parse_key(folder: &str, key: &str) -> Option<ParsedKey> {
    let rest = key.strip_prefix(&folder_prefix(folder))?;
    let (source, remainder) = rest.split_once('/')?;
    validate_name("source", source).ok()?;
    let kind = if remainder == latest_file_name(source) {
        BackupKind::Latest
    } else {
        BackupKind::Versioned {
            stamp: parse_versioned_file(source, remainder)?,
        }
    };
    Some(ParsedKey {
        source: source.to_string(),
        kind,
    })
}

/// `versioned/<source>.<stamp>.tar.gz` → the stamp, if exact.
fn parse_versioned_file(source: &str, remainder: &str) -> Option<String> {
    let file = remainder.strip_prefix("versioned/")?;
    if file.contains('/') {
        return None;
    }
    let stamp = file
        .strip_prefix(source)?
        .strip_prefix('.')?
        .strip_suffix(ARCHIVE_SUFFIX)?;
    parse_stamp(stamp)?;
    Some(stamp.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn stamp_renders_in_timezone_and_round_trips() {
        let utc = Utc.with_ymd_and_hms(2026, 9, 4, 2, 15, 7).single().unwrap();
        let berlin: Tz = match "Europe/Berlin".parse() {
            Ok(tz) => tz,
            Err(e) => panic!("{e}"),
        };
        let stamp = render_stamp(utc, berlin);
        assert_eq!(stamp, "2026-09-04-041507");
        let parsed = parse_stamp(&stamp).unwrap();
        assert_eq!(parsed.format(STAMP_FORMAT).to_string(), stamp);
        assert!(parse_stamp("2026-13-99-000000").is_none());
        assert!(parse_stamp("not-a-stamp").is_none());
    }

    #[test]
    fn keys_follow_the_layout() {
        assert_eq!(
            versioned_key("dbbackup", "appdb", "2026-09-04-041507"),
            "dbbackup/appdb/versioned/appdb.2026-09-04-041507.tar.gz"
        );
        assert_eq!(
            latest_key("/dbbackup/", "appdb"),
            "dbbackup/appdb/appdb.latest.tar.gz"
        );
        assert_eq!(latest_key("", "appdb"), "appdb/appdb.latest.tar.gz");
        assert_eq!(versioned_prefix("f", "s"), "f/s/versioned/");
    }

    #[test]
    fn parse_key_accepts_only_the_exact_layout() {
        let v = parse_key(
            "dbbackup",
            "dbbackup/appdb/versioned/appdb.2026-09-04-041507.tar.gz",
        )
        .unwrap();
        assert_eq!(v.source, "appdb");
        assert_eq!(
            v.kind,
            BackupKind::Versioned {
                stamp: "2026-09-04-041507".into()
            }
        );
        let l = parse_key("dbbackup", "dbbackup/appdb/appdb.latest.tar.gz").unwrap();
        assert_eq!(l.kind, BackupKind::Latest);

        for unparsable in [
            "archive/appdb/logs/logs.2026-04.parquet",
            "dbbackup/appdb/versioned/other.2026-09-04-041507.tar.gz",
            "dbbackup/appdb/versioned/appdb.garbage.tar.gz",
            "dbbackup/appdb/versioned/nested/appdb.2026-09-04-041507.tar.gz",
            "dbbackup/appdb/appdb.2026-09-04-041507.tar.gz",
            "elsewhere/appdb/appdb.latest.tar.gz",
            "dbbackup/bad name/bad name.latest.tar.gz",
            "dbbackup/appdb/versioned/appdb.2026-09-04-041507.tar.gz.tmp",
        ] {
            assert!(parse_key("dbbackup", unparsable).is_none(), "{unparsable}");
        }
    }
}
