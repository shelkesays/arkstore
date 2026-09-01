//! Concurrency limits: how many sources run in parallel (I/O-bound) and how many
//! CPU workers handle compression / Parquet encoding (CPU-bound).
//!
//! The two are separate on purpose. Source parallelism is bounded to protect the
//! database and object store — *not* the local core count, since the per-source
//! work is mostly network wait. CPU workers are bounded by available cores, where
//! oversubscription only adds contention.

use std::num::NonZeroUsize;

use serde::{Deserialize, Deserializer};

/// A configurable limit: `auto` (resolved from hardware / a safe default) or a
/// fixed positive count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Limit {
    #[default]
    Auto,
    Fixed(usize),
}

/// Accepts either the keyword `auto` or a positive integer in YAML.
#[derive(Deserialize)]
#[serde(untagged)]
enum LimitRepr {
    Number(usize),
    Keyword(String),
}

impl<'de> Deserialize<'de> for Limit {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match LimitRepr::deserialize(deserializer)? {
            LimitRepr::Number(0) => Err(serde::de::Error::custom("concurrency limit must be >= 1")),
            LimitRepr::Number(n) => Ok(Limit::Fixed(n)),
            LimitRepr::Keyword(s) if s.eq_ignore_ascii_case("auto") => Ok(Limit::Auto),
            LimitRepr::Keyword(s) => Err(serde::de::Error::custom(format!(
                "expected `auto` or a positive integer, got `{s}`"
            ))),
        }
    }
}

/// The `concurrency:` policy block.
#[derive(Debug, Clone, Copy, Deserialize, Default)]
#[serde(default)]
pub struct Concurrency {
    /// Sources processed in parallel. `auto` is a conservative default that
    /// protects a shared database — not the local core count.
    pub max_sources: Limit,
    /// Parallel CPU workers for compression / Parquet encoding. `auto` = the
    /// number of available cores; a fixed value is clamped to the core count.
    pub cpu_workers: Limit,
}

impl Concurrency {
    /// Resolve `auto`/fixed limits into concrete worker counts for this host.
    pub fn resolved(&self) -> Resolved {
        let cores = available_cores();
        Resolved {
            max_sources: match self.max_sources {
                // I/O-bound: a small default that won't overwhelm a shared DB.
                Limit::Auto => 4,
                Limit::Fixed(n) => n.max(1),
            },
            cpu_workers: match self.cpu_workers {
                // CPU-bound: cores is the natural ceiling; clamp fixed values to it.
                Limit::Auto => cores,
                Limit::Fixed(n) => n.clamp(1, cores),
            },
        }
    }
}

/// Concrete, resolved worker counts for the current host.
#[derive(Debug, Clone, Copy)]
pub struct Resolved {
    pub max_sources: usize,
    pub cpu_workers: usize,
}

/// Available parallelism (logical cores), or 1 if it cannot be determined.
fn available_cores() -> usize {
    std::thread::available_parallelism()
        .map(NonZeroUsize::get)
        .unwrap_or(1)
}
