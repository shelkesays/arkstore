//! Quoting for emitted DDL and catalog-driven statements. Every identifier
//! or literal that reaches SQL text passes through here — the only way a
//! catalog name becomes part of a statement.

/// Double-quote an identifier, doubling embedded quotes. Always quotes, so
/// case, spaces, and reserved words survive unchanged.
pub fn quote(ident: &str) -> String {
    let mut out = String::with_capacity(ident.len().saturating_add(2));
    out.push('"');
    for ch in ident.chars() {
        if ch == '"' {
            out.push('"');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// `"schema"."name"`.
pub fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote(schema), quote(name))
}

/// Single-quote a string literal under `standard_conforming_strings = on`.
pub fn escape(literal: &str) -> String {
    format!("'{}'", literal.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_always_quotes_and_doubles_embedded_quotes() {
        assert_eq!(quote("orders"), "\"orders\"");
        assert_eq!(quote("Mixed Case"), "\"Mixed Case\"");
        assert_eq!(quote("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(qualified("public", "t"), "\"public\".\"t\"");
    }

    #[test]
    fn escape_doubles_single_quotes() {
        assert_eq!(escape("it's"), "'it''s'");
        assert_eq!(escape("plain"), "'plain'");
    }
}
