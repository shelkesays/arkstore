#![forbid(unsafe_code)]

//! Arkstore — backup, restore, retention cleanup, cold-tier archival, and
//! verification for databases and files against S3-compatible object storage.
//!
//! This crate is the library core; the `arkstore` binary is a thin CLI over
//! it. See `PRD.md` and `docs/knowledge-base.md` for the design.

pub mod cli;
pub mod config;
pub mod engine;
pub mod error;
pub mod hash;
pub mod layout;
pub mod manifest;
pub mod ops;
pub mod pack;
pub mod redact;
pub mod secrets;
pub mod store;

pub use error::{ArkError, Result};
