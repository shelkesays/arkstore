//! Native PostgreSQL backend (PRD §5.1, KB §11): one `REPEATABLE READ`
//! snapshot per source, DDL assembled from `pg_catalog`, data streamed with
//! `COPY … TO STDOUT`, and a `manifest.json` recording the dependency graph,
//! row counts, and content hashes.

mod catalog;
mod conn;
mod ddl;
mod dump;
mod sql;

pub use conn::{MIN_SERVER_VERSION_NUM, SESSION_SETTINGS};
pub use dump::{dump, preview};
