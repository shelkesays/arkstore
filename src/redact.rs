//! Redaction of credentials from text bound for logs, summaries, or an
//! error-reporting sink. Driver and object-store error text passes through
//! here before it is shown anywhere (PRD §9.6).

const MASK: &str = "***";
const KEYS: [&str; 5] = ["password", "passwd", "pwd", "secret", "token"];

/// Redact credentials in `text`: `password=…`-style pairs (any case; `=` or
/// `:` separated) and the password part of URL userinfo (`user:pass@host`).
pub fn redact(text: &str) -> String {
    redact_url_userinfo(&redact_key_values(text))
}

fn redact_key_values(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some(value_end) = key_value_end(&lower, bytes, i) {
            let value_start = value_end.0;
            out.push_str(&text[i..value_start]);
            out.push_str(MASK);
            i = value_end.1;
            continue;
        }
        let len = utf8_len(bytes[i]);
        out.push_str(&text[i..i.saturating_add(len)]);
        i = i.saturating_add(len);
    }
    out
}

/// If a credential key starts at `i`, return `(value_start, value_end)`.
fn key_value_end(lower: &str, bytes: &[u8], i: usize) -> Option<(usize, usize)> {
    let rest = lower.get(i..)?;
    let key = KEYS.iter().find(|k| rest.starts_with(*k))?;
    if i > 0 && is_word_byte(bytes[i.saturating_sub(1)]) {
        return None;
    }
    let mut j = i.saturating_add(key.len());
    while bytes.get(j) == Some(&b' ') {
        j = j.saturating_add(1);
    }
    match bytes.get(j) {
        Some(b'=') | Some(b':') => j = j.saturating_add(1),
        _ => return None,
    }
    while bytes.get(j) == Some(&b' ') {
        j = j.saturating_add(1);
    }
    let start = j;
    while bytes.get(j).is_some_and(|b| !is_terminator(*b)) {
        j = j.saturating_add(1);
    }
    (j > start).then_some((start, j))
}

fn redact_url_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(scheme_end) = rest.find("://") {
        let after = scheme_end.saturating_add(3);
        out.push_str(&rest[..after]);
        rest = &rest[after..];
        let authority_end = rest
            .find(|c: char| c == '/' || c.is_whitespace() || c == '?' || c == '#')
            .unwrap_or(rest.len());
        push_redacted_authority(&rest[..authority_end], &mut out);
        rest = &rest[authority_end..];
    }
    out.push_str(rest);
    out
}

/// Copy `user[:pass]@host…` to `out` with the password masked.
fn push_redacted_authority(authority: &str, out: &mut String) {
    let Some(at) = authority.rfind('@') else {
        out.push_str(authority);
        return;
    };
    let userinfo = &authority[..at];
    match userinfo.find(':') {
        Some(colon) => {
            out.push_str(&userinfo[..colon]);
            out.push(':');
            out.push_str(MASK);
        }
        None => out.push_str(userinfo),
    }
    out.push_str(&authority[at..]);
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn is_terminator(b: u8) -> bool {
    b.is_ascii_whitespace() || matches!(b, b'&' | b';' | b',' | b'"' | b'\'' | b')' | b'}' | b']')
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_key_value_pairs_any_case() {
        assert_eq!(
            redact("host=db password=s3cr3t dbname=x"),
            "host=db password=*** dbname=x"
        );
        assert_eq!(
            redact("PASSWORD: hunter2, user: bob"),
            "PASSWORD: ***, user: bob"
        );
        assert_eq!(redact("token=abc&x=1"), "token=***&x=1");
    }

    #[test]
    fn masks_url_userinfo_password_only() {
        assert_eq!(
            redact("postgres://alice:pw@db.internal:5432/appdb?sslmode=require"),
            "postgres://alice:***@db.internal:5432/appdb?sslmode=require"
        );
        assert_eq!(redact("mongodb://bob@host/db"), "mongodb://bob@host/db");
        assert_eq!(
            redact("https://example.com/path"),
            "https://example.com/path"
        );
    }

    #[test]
    fn leaves_ordinary_text_alone() {
        let s = "connection refused to db.internal:5432 (passwordless auth not enabled)";
        assert_eq!(redact(s), s);
        assert_eq!(redact("tokenizer=fast"), "tokenizer=fast");
        assert_eq!(redact("héllo pwd=ünïcode ok"), "héllo pwd=*** ok");
    }
}
