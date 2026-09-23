//! Native PostgreSQL restore (KB §5.4–5.7): prove the target is empty, check
//! every file the manifest promises, then load in load-plan order —
//! structure, data by `COPY … FROM STDIN`, triggers, post-data (foreign
//! keys, matview refresh) — with per-object failure isolation and a presence
//! check at the end. The manifest is the authority throughout.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use bytes::Bytes;
use futures::SinkExt;
use tokio::io::AsyncReadExt;
use tracing::{debug, info, warn};

use super::catalog::{not_ext, USER_SCHEMA};
use super::conn::Conn;
use super::sql::qualified;
use crate::config::ResolvedTarget;
use crate::engine::{RestoreOutcome, TargetContents};
use crate::error::{ArkError, Result};
use crate::manifest::{FileEntry, FileRole, Manifest, ObjectEntry, ObjectKind};
use crate::pack::{digest_file, sanitize};

/// Bytes read from a data file per `COPY` chunk.
const COPY_CHUNK: usize = 1024 * 1024;

/// User objects in the target: relations of every kind, functions, and
/// user types in non-system schemas. Extension-owned objects are not
/// counted — they belong to their extension, which the archive recreates
/// with `CREATE EXTENSION IF NOT EXISTS`.
pub async fn target_contents(target: &ResolvedTarget) -> Result<TargetContents> {
    let conn = Conn::connect_target(target).await?;
    let sql = format!(
        "WITH objs AS ( \
           SELECT n.nspname || '.' || c.relname AS name FROM pg_catalog.pg_class c \
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p', 'v', 'm', 'S', 'c', 'f') AND {USER_SCHEMA} AND {ext_class} \
           UNION ALL SELECT n.nspname || '.' || p.proname FROM pg_catalog.pg_proc p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE {USER_SCHEMA} AND {ext_proc} \
           UNION ALL SELECT n.nspname || '.' || t.typname FROM pg_catalog.pg_type t \
             JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
             WHERE t.typtype IN ('e', 'd', 'r') AND {USER_SCHEMA} AND {ext_type} \
         ) SELECT count(*) OVER (), name::text FROM objs ORDER BY name LIMIT 5",
        ext_class = not_ext("pg_class", "c.oid"),
        ext_proc = not_ext("pg_proc", "p.oid"),
        ext_type = not_ext("pg_type", "t.oid"),
    );
    let rows = conn.rows(&sql, &[]).await?;
    let mut contents = TargetContents::default();
    for row in &rows {
        contents.total = row
            .try_get(0)
            .map_err(|e| super::conn::engine_err("cannot read target contents", e))?;
        let name: String = row
            .try_get(1)
            .map_err(|e| super::conn::engine_err("cannot read target contents", e))?;
        contents.sample.push(name);
    }
    Ok(contents)
}

/// Whether the target already defines `object`; `None` when the kind cannot
/// be looked up on this engine.
pub async fn target_defines(target: &ResolvedTarget, object: &ObjectEntry) -> Result<Option<bool>> {
    let conn = Conn::connect_target(target).await?;
    object_exists(&conn, object).await
}

async fn relation_exists(conn: &Conn, name: &str) -> Result<bool> {
    let (schema, object) = split_name(name);
    exists(
        conn,
        "SELECT pg_catalog.to_regclass($1) IS NOT NULL",
        &[qualified(schema, object)],
    )
    .await
}

/// Existence lookup per object kind (KB §5.4): relations by `to_regclass`,
/// types by `to_regtype`, functions by `to_regprocedure` on their identity
/// arguments, triggers in `pg_trigger`, schemas and extensions by name.
async fn object_exists(conn: &Conn, object: &ObjectEntry) -> Result<Option<bool>> {
    let Some((sql, params)) = existence_query(object) else {
        return Ok(None);
    };
    exists(conn, sql, &params).await.map(Some)
}

fn existence_query(object: &ObjectEntry) -> Option<(&'static str, Vec<String>)> {
    let name = object.name.as_str();
    let (schema, rest) = split_name(name);
    Some(match object.kind {
        ObjectKind::Table | ObjectKind::View | ObjectKind::Matview | ObjectKind::Sequence => (
            "SELECT pg_catalog.to_regclass($1) IS NOT NULL",
            vec![qualified(schema, rest)],
        ),
        ObjectKind::Type => (
            "SELECT pg_catalog.to_regtype($1) IS NOT NULL",
            vec![qualified(schema, rest)],
        ),
        ObjectKind::Function => {
            // Manifest names carry the identity arguments exactly as
            // `pg_get_function_identity_arguments` renders them.
            let (function, args) = rest.split_once('(').unwrap_or((rest, ")"));
            (
                "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_proc p \
                 JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
                 WHERE n.nspname = $1 AND p.proname = $2 \
                 AND pg_catalog.pg_get_function_identity_arguments(p.oid) = $3)",
                vec![
                    schema.to_string(),
                    function.to_string(),
                    args.trim_end_matches(')').to_string(),
                ],
            )
        }
        ObjectKind::Trigger => {
            let (table, trigger) = rest.rsplit_once('.').unwrap_or((rest, ""));
            (
                "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_trigger t \
                 WHERE t.tgrelid = pg_catalog.to_regclass($1) AND t.tgname = $2 AND NOT t.tgisinternal)",
                vec![qualified(schema, table), trigger.to_string()],
            )
        }
        ObjectKind::Schema => (
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = $1)",
            vec![name.to_string()],
        ),
        ObjectKind::Extension => (
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_extension WHERE extname = $1)",
            vec![name.strip_prefix("extension:").unwrap_or(name).to_string()],
        ),
        ObjectKind::Collection | ObjectKind::MongoView => return None,
    })
}

async fn exists(conn: &Conn, sql: &str, params: &[String]) -> Result<bool> {
    let refs: Vec<&(dyn tokio_postgres::types::ToSql + Sync)> = params
        .iter()
        .map(|p| p as &(dyn tokio_postgres::types::ToSql + Sync))
        .collect();
    let rows = conn.rows(sql, &refs).await?;
    let row = rows.first().ok_or_else(|| ArkError::Engine {
        engine: "PostgreSQL",
        message: "existence query returned no row".into(),
    })?;
    row.try_get(0)
        .map_err(|e| super::conn::engine_err("cannot read existence query", e))
}

/// `schema.object` → (`schema`, `object`), split at the first dot (KB §2.5).
fn split_name(name: &str) -> (&str, &str) {
    name.split_once('.').unwrap_or(("public", name))
}

/// Load the unpacked archive in `dir` into `target`.
pub async fn restore(
    target: &ResolvedTarget,
    dir: &Path,
    manifest: &Manifest,
) -> Result<RestoreOutcome> {
    let conn = Conn::connect_target(target).await?;
    let mut loader = Loader {
        conn,
        dir,
        manifest,
        intact: HashMap::new(),
        failed: HashSet::new(),
        applied: HashSet::new(),
        outcome: RestoreOutcome::default(),
    };
    loader.check_files();
    loader.prepare_session().await?;
    let order = ordered(manifest);
    for object in &order {
        if object.kind != ObjectKind::Trigger {
            loader.apply_structure(object).await;
        }
    }
    for object in &order {
        loader.load_data(object).await;
    }
    for object in &order {
        if object.kind == ObjectKind::Trigger {
            loader.apply_structure(object).await;
        }
    }
    for object in &order {
        loader.apply_post(object).await;
    }
    for object in &order {
        loader.verify_presence(object).await;
    }
    Ok(loader.finish())
}

/// Load-plan layers flattened, then the cyclic objects (their foreign keys
/// live in post-data files, so creation order among them is free).
fn ordered(manifest: &Manifest) -> Vec<&ObjectEntry> {
    let plan = manifest.load_plan();
    let mut order: Vec<&ObjectEntry> = plan.layers.into_iter().flatten().collect();
    order.extend(plan.cyclic);
    order
}

struct Loader<'a> {
    conn: Conn,
    dir: &'a Path,
    manifest: &'a Manifest,
    /// Archive path → the file exists with the recorded size and digest.
    intact: HashMap<String, bool>,
    failed: HashSet<String>,
    /// Objects with at least one file applied.
    applied: HashSet<String>,
    outcome: RestoreOutcome,
}

impl Loader<'_> {
    fn fail(&mut self, object: &str, reason: String) {
        if self.failed.insert(object.to_string()) {
            warn!(object, %reason, "object failed; continuing with the rest");
            self.outcome.failed.push((object.to_string(), reason));
        }
    }

    fn is_failed(&self, object: &ObjectEntry) -> bool {
        self.failed.contains(&object.name)
    }

    /// Compare every listed file with its recorded size and digest. A data
    /// or post-data file that is missing or corrupt fails its object now; a
    /// structure file is judged when it is applied (KB §5.4 step 5).
    fn check_files(&mut self) {
        let entries: Vec<(String, FileEntry)> = self
            .manifest
            .objects
            .iter()
            .flat_map(|o| o.files.iter().map(move |f| (o.name.clone(), f.clone())))
            .collect();
        for (object, file) in entries {
            let ok = file_intact(self.dir, &file);
            self.intact.insert(file.path.clone(), ok);
            if !ok && file.role != FileRole::Structure {
                self.fail(
                    &object,
                    format!("`{}` is missing or does not match the manifest", file.path),
                );
            }
        }
    }

    /// `check_function_bodies` off (KB §5.6); trigger suppression attempted
    /// and dropped on a permission error.
    async fn prepare_session(&self) -> Result<()> {
        self.conn.exec("SET check_function_bodies = off").await?;
        match self
            .conn
            .exec("SET session_replication_role = replica")
            .await
        {
            Ok(()) => debug!("session_replication_role = replica"),
            Err(e) => info!(error = %e, "loading without session_replication_role = replica"),
        }
        Ok(())
    }

    async fn apply_structure(&mut self, object: &ObjectEntry) {
        if self.is_failed(object) {
            return;
        }
        let structure = object
            .files
            .iter()
            .find(|f| f.role == FileRole::Structure)
            .cloned();
        match structure {
            Some(file) if self.intact.get(&file.path) == Some(&true) => {
                self.apply_sql(object, &file.path).await;
            }
            Some(file) => {
                let reason = format!("structure file `{}` is missing or corrupt", file.path);
                self.require_existing(object, reason).await;
            }
            None if object.kind == ObjectKind::Table => {
                self.require_existing(object, "no structure file (data-only)".into())
                    .await;
            }
            None => {}
        }
    }

    /// A table without usable DDL is acceptable only if the target already
    /// defines it (KB §5.7 data-only); anything else fails.
    async fn require_existing(&mut self, object: &ObjectEntry, reason: String) {
        if object.kind != ObjectKind::Table {
            self.fail(&object.name, reason);
            return;
        }
        match relation_exists(&self.conn, &object.name).await {
            Ok(true) => {
                info!(object = %object.name, %reason, "target already defines it; loading rows into the existing table")
            }
            Ok(false) => self.fail(
                &object.name,
                format!("{reason} and the target does not define it"),
            ),
            Err(e) => self.fail(&object.name, e.to_string()),
        }
    }

    async fn apply_sql(&mut self, object: &ObjectEntry, path: &str) {
        let text = match std::fs::read_to_string(self.dir.join(path)) {
            Ok(text) => text,
            Err(e) => return self.fail(&object.name, format!("cannot read `{path}`: {e}")),
        };
        match self.conn.exec(&text).await {
            Ok(()) => {
                self.applied.insert(object.name.clone());
                debug!(object = %object.name, file = path, "applied");
            }
            Err(e) => self.fail(&object.name, format!("`{path}`: {e}")),
        }
    }

    async fn load_data(&mut self, object: &ObjectEntry) {
        if self.is_failed(object) {
            return;
        }
        let Some(file) = object.files.iter().find(|f| f.role == FileRole::Data) else {
            return;
        };
        let (schema, name) = split_name(&object.name);
        let table = qualified(schema, name);
        if let Err(e) = self.conn.exec("BEGIN").await {
            return self.fail(&object.name, e.to_string());
        }
        // The load is one transaction: a count mismatch or a COPY error rolls
        // it back, so a failed object leaves no rows behind.
        let verdict = match copy_in(&self.conn, &table, &self.dir.join(&file.path)).await {
            Ok(rows) if Some(rows) == object.row_count || object.row_count.is_none() => Ok(rows),
            Ok(rows) => Err(format!(
                "loaded {rows} rows but the manifest recorded {}",
                object.row_count.unwrap_or_default()
            )),
            Err(e) => Err(format!("`{}`: {e}", file.path)),
        };
        if let Err(reason) = &verdict {
            self.fail(&object.name, reason.clone());
        }
        let committed = self.end_transaction(&object.name).await;
        if let (Ok(rows), true) = (verdict, committed) {
            self.applied.insert(object.name.clone());
            debug!(object = %object.name, rows, "rows loaded");
        }
    }

    /// Commit the data transaction, or roll it back when the object already
    /// failed. Returns whether the object is still good.
    async fn end_transaction(&mut self, object: &str) -> bool {
        let statement = if self.failed.contains(object) {
            "ROLLBACK"
        } else {
            "COMMIT"
        };
        match self.conn.exec(statement).await {
            Ok(()) => !self.failed.contains(object),
            Err(e) => {
                self.fail(object, format!("{statement} failed: {e}"));
                false
            }
        }
    }

    async fn apply_post(&mut self, object: &ObjectEntry) {
        if self.is_failed(object) {
            return;
        }
        let post: Vec<String> = object
            .files
            .iter()
            .filter(|f| f.role == FileRole::PostData)
            .map(|f| f.path.clone())
            .collect();
        for path in post {
            self.apply_sql(object, &path).await;
        }
    }

    /// After loading, every object the engine can look up must exist
    /// (PRD §6.2 step 7).
    async fn verify_presence(&mut self, object: &ObjectEntry) {
        if self.is_failed(object) {
            return;
        }
        match object_exists(&self.conn, object).await {
            Ok(Some(true)) | Ok(None) => {}
            Ok(Some(false)) => self.fail(&object.name, "missing from the target after load".into()),
            Err(e) => self.fail(&object.name, e.to_string()),
        }
    }

    fn finish(mut self) -> RestoreOutcome {
        for object in &self.manifest.objects {
            if self.failed.contains(&object.name) {
                continue;
            }
            if self.applied.contains(&object.name) {
                self.outcome.restored.push(object.name.clone());
            } else {
                self.outcome.skipped.push(object.name.clone());
            }
        }
        self.outcome
    }
}

/// Whether `file` exists under `dir` with the recorded size and digest.
fn file_intact(dir: &Path, file: &FileEntry) -> bool {
    match digest_file(&dir.join(&file.path)) {
        Ok((size, sha)) => size == file.size && sha == file.sha256,
        Err(e) => {
            debug!(file = %file.path, error = %e, "archive file cannot be digested");
            false
        }
    }
}

/// Stream a text-format data file into `COPY table FROM STDIN`; returns the
/// row count the server reports.
async fn copy_in(conn: &Conn, table: &str, path: &Path) -> Result<u64> {
    let clean = sanitize(path)?;
    let mut file = tokio::fs::File::open(&clean).await?;
    let sink = conn
        .copy_in(&format!("COPY {table} FROM STDIN (FORMAT text)"))
        .await?;
    let mut sink = std::pin::pin!(sink);
    let mut buf = vec![0u8; COPY_CHUNK];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let chunk = Bytes::copy_from_slice(buf.get(..n).unwrap_or_default());
        sink.send(chunk)
            .await
            .map_err(|e| super::conn::engine_err("COPY FROM stream failed", e))?;
    }
    sink.as_mut()
        .finish()
        .await
        .map_err(|e| super::conn::engine_err("COPY FROM failed", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_split_at_the_first_dot() {
        assert_eq!(split_name("shop.orders"), ("shop", "orders"));
        assert_eq!(split_name("shop.Mixed Case"), ("shop", "Mixed Case"));
        assert_eq!(split_name("shop.a.b"), ("shop", "a.b"));
        assert_eq!(split_name("bare"), ("public", "bare"));
    }
}
