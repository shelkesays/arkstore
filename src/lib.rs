#![forbid(unsafe_code)]

//! Arkstore — backup, restore, retention cleanup, and cold-tier archival for
//! databases and files against S3-compatible object storage.
//!
//! This crate is the library core; the `arkstore` binary ([`main`]) is a thin
//! CLI over it. See `PRD.md` and `docs/knowledge-base.md` for the design.

pub mod cli;
pub mod config;
pub mod engine;
pub mod error;
pub mod ops;
pub mod secrets;
pub mod store;

pub use error::{ArkError, Result};
