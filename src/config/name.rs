//! Source / target name grammar (PRD §7): a safe single path segment.
//!
//! `^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$`, ASCII-only by design, no `..`. Names
//! become S3 key components and local directory / file names, so anything else
//! is rejected up front.

use crate::error::{ArkError, Result};

/// Maximum name length in characters.
pub const MAX_NAME_LEN: usize = 64;

/// Validate `name` against the grammar; the error names the offending value.
pub fn validate_name(kind: &str, name: &str) -> Result<()> {
    let reject = |why: &str| {
        Err(ArkError::Validation(format!(
            "{kind} name `{name}` is invalid: {why} \
             (allowed: 1-64 ASCII letters/digits/`_`/`.`/`-`, starting with a letter or digit)"
        )))
    };
    if name.is_empty() {
        return reject("it is empty");
    }
    if name.len() > MAX_NAME_LEN {
        return reject("it is longer than 64 characters");
    }
    if name.contains("..") {
        return reject("it contains `..`");
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return reject("it must start with an ASCII letter or digit"),
    }
    if let Some(bad) = chars.find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
    {
        return reject(&format!("it contains the character `{bad}`"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_valid_names() {
        for ok in ["appdb", "a", "A1", "app-db_v2.prod", &"x".repeat(64)] {
            assert!(validate_name("source", ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn rejects_invalid_names() {
        for bad in [
            "",
            "-lead",
            ".lead",
            "_lead",
            "has space",
            "has/slash",
            "has\\backslash",
            "a..b",
            "ünicode",
            &"x".repeat(65),
        ] {
            assert!(validate_name("source", bad).is_err(), "{bad:?} should fail");
        }
    }
}
