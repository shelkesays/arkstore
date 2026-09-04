//! Hashing primitives: file digests, the canonical-definition `schema_hash`,
//! and the order-independent per-table content hash (KB §11.3).

use sha2::{Digest, Sha256};

/// Lower-case hex encoding.
pub fn hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for b in bytes {
        out.push(char::from(TABLE[usize::from(b >> 4)]));
        out.push(char::from(TABLE[usize::from(b & 0x0f)]));
    }
    out
}

/// Bare hex SHA-256 of `bytes` — used for per-file digests in the manifest.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// `sha256:<hex>` of an object's canonicalised definition (manifest
/// `schema_hash`). Callers pass already-canonical text.
pub fn schema_hash(canonical: &str) -> String {
    format!("sha256:{}", sha256_hex(canonical.as_bytes()))
}

/// Streaming, order-independent, duplicate-sensitive content hash:
/// `Σ SHA-256(row) mod 2^256`, rendered as `sum256:<64 hex>`.
///
/// Addition is commutative, so `verify` can recompute it from a restored
/// target in any row order and batch size; every row contributes, so dropped or
/// duplicated rows change the value. A corruption detector, not a commitment.
#[derive(Debug, Clone, Default)]
pub struct Sum256 {
    acc: [u8; 32],
    rows: u64,
}

impl Sum256 {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one canonical row (without its trailing newline) into the sum.
    pub fn add_row(&mut self, row: &[u8]) {
        let digest = Sha256::digest(row);
        let mut carry: u16 = 0;
        for (a, d) in self.acc.iter_mut().rev().zip(digest.iter().rev()) {
            let sum = u16::from(*a)
                .saturating_add(u16::from(*d))
                .saturating_add(carry);
            *a = sum.to_le_bytes()[0];
            carry = sum >> 8;
        }
        self.rows = self.rows.saturating_add(1);
    }

    /// Rows folded so far.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// The `sum256:<hex>` rendering of the current sum.
    pub fn finish(&self) -> String {
        format!("sum256:{}", hex(&self.acc))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip_known_vector() {
        assert_eq!(hex(&[0x00, 0xab, 0xff]), "00abff");
        // SHA-256("") — well-known constant.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(schema_hash("CREATE TABLE t (id int)").starts_with("sha256:"));
    }

    #[test]
    fn sum256_is_order_independent() {
        let mut a = Sum256::new();
        let mut b = Sum256::new();
        for r in ["1\talice", "2\tbob", "3\tcarol"] {
            a.add_row(r.as_bytes());
        }
        for r in ["3\tcarol", "1\talice", "2\tbob"] {
            b.add_row(r.as_bytes());
        }
        assert_eq!(a.finish(), b.finish());
        assert_eq!(a.rows(), 3);
    }

    #[test]
    fn sum256_detects_drops_and_duplicates() {
        let mut full = Sum256::new();
        let mut short = Sum256::new();
        let mut dup = Sum256::new();
        for r in ["1\talice", "2\tbob"] {
            full.add_row(r.as_bytes());
            dup.add_row(r.as_bytes());
        }
        short.add_row(b"1\talice");
        dup.add_row(b"2\tbob");
        assert_ne!(full.finish(), short.finish());
        assert_ne!(full.finish(), dup.finish());
        assert_eq!(Sum256::new().finish(), format!("sum256:{}", "0".repeat(64)));
    }

    #[test]
    fn sum256_carries_across_bytes() {
        // Adding the same digest 300 times must overflow individual bytes and
        // carry; the result must still be deterministic and 64 hex chars.
        let mut s = Sum256::new();
        for _ in 0..300 {
            s.add_row(b"same");
        }
        let h = s.finish();
        assert_eq!(h.len(), "sum256:".len() + 64);
        let mut t = Sum256::new();
        for _ in 0..300 {
            t.add_row(b"same");
        }
        assert_eq!(h, t.finish());
    }
}
