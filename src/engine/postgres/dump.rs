//! The dump pipeline (KB §2.1): open the snapshot, read the catalog, apply
//! the ignore rules, write one structure file (plus a data file for tables
//! and a post-data file for foreign keys / matview refresh) per object, and
//! assemble `manifest.json` with the
//! dependency graph, row counts, and content hashes.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use chrono::Utc;
use futures::TryStreamExt;
use tokio::io::{AsyncWriteExt, BufWriter};
use tracing::{debug, info, warn};

use super::catalog::{Catalog, RelKind};
use super::conn::{Conn, SESSION_SETTINGS};
use super::ddl::{Emitter, Script};
use super::sql::qualified;
use crate::config::{CopyFormat, Source};
use crate::engine::{DumpContext, DumpPreview};
use crate::error::{ArkError, Result};
use crate::hash::{sha256_hex, Sum256};
use crate::manifest::{
    Consistency, FileEntry, FileRole, Manifest, ObjectEntry, ObjectKind, Snapshot, MANIFEST_VERSION,
};
use crate::pack::digest_file;

/// Which catalog entry an object comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum What {
    Schema(usize),
    Extension(usize),
    Type(usize),
    Sequence(usize),
    Relation(usize),
    Function(usize),
    Trigger(usize),
}

/// One object to dump.
#[derive(Debug, Clone)]
struct Item {
    name: String,
    kind: ObjectKind,
    stem: String,
    what: What,
    /// Table data is skipped (`ignore`) — structure only.
    data_skipped: bool,
}

/// The objects to dump, after ignore rules, plus lookups for dependencies.
struct Plan {
    items: Vec<Item>,
    /// (`pg_class` | `pg_type` | `pg_proc` | `pg_namespace` | `pg_trigger`, oid) → item.
    index: HashMap<(&'static str, u32), usize>,
}

/// Dump `source` into `ctx.work_dir` and return the manifest written there.
pub async fn dump(source: &Source, ctx: &DumpContext<'_>) -> Result<Manifest> {
    if source.effective_copy_format() == CopyFormat::Binary {
        return Err(ArkError::NotImplemented(
            "copy_format: binary (text COPY is the v1 data format)",
        ));
    }
    let conn = Conn::open(source).await?;
    let snapshot_id = conn.begin_snapshot().await?;
    let created_at = Utc::now();
    let catalog = Catalog::load(&conn, source.effective_include_privileges()).await?;
    log_unsupported(source, &catalog);
    let plan = Plan::build(source, &catalog);
    let emitter = Emitter {
        catalog: &catalog,
        include_privileges: source.effective_include_privileges(),
        rel_names: rel_names(&catalog),
    };
    let mut objects = Vec::with_capacity(plan.items.len());
    for (i, item) in plan.items.iter().enumerate() {
        let depends_on = plan.depends_on(i, &catalog);
        objects.push(write_object(&conn, source, ctx.work_dir, item, &emitter, depends_on).await?);
    }
    conn.end_snapshot().await?;
    let manifest = Manifest {
        manifest_version: MANIFEST_VERSION,
        source: source.name.clone(),
        engine: source.source_type,
        server_version: conn.server_version.clone(),
        created_at,
        stamp: ctx.stamp.to_string(),
        timezone: ctx.timezone.to_string(),
        snapshot: Snapshot {
            kind: "pg_snapshot".into(),
            id: Some(snapshot_id),
        },
        consistent: true,
        session: SESSION_SETTINGS
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect::<BTreeMap<_, _>>(),
        objects,
    };
    manifest.validate()?;
    std::fs::write(ctx.work_dir.join("manifest.json"), manifest.to_json()?)?;
    info!(source = %source.name, objects = manifest.objects.len(), server = %conn.server_version, "dump complete");
    Ok(manifest)
}

/// Connect, snapshot, enumerate — report counts without writing.
pub async fn preview(source: &Source) -> Result<DumpPreview> {
    let conn = Conn::open(source).await?;
    conn.begin_snapshot().await?;
    let catalog = Catalog::load(&conn, source.effective_include_privileges()).await?;
    log_unsupported(source, &catalog);
    let plan = Plan::build(source, &catalog);
    conn.end_snapshot().await?;
    let tables = plan
        .items
        .iter()
        .filter(|i| i.kind == ObjectKind::Table)
        .count();
    let skipped = plan.items.iter().filter(|i| i.data_skipped).count();
    Ok(DumpPreview {
        server_version: conn.server_version.clone(),
        objects: plan.items.len(),
        tables_with_data: tables.saturating_sub(skipped),
        data_skipped: skipped,
    })
}

fn log_unsupported(source: &Source, catalog: &Catalog) {
    for (kind, count) in &catalog.unsupported {
        warn!(source = %source.name, kind = %kind, count, "objects outside the fidelity contract are not dumped");
    }
}

fn rel_names(catalog: &Catalog) -> HashMap<u32, String> {
    catalog
        .relations
        .iter()
        .map(|r| (r.oid, qualified(&r.schema, &r.name)))
        .collect()
}

/// Ignore rules (KB §2.3): prefixes exclude outright; `ignore` skips a
/// table's data but keeps its structure.
struct Rules {
    prefixes: Vec<String>,
    data_skip: Vec<String>,
}

impl Rules {
    fn from_source(source: &Source) -> Self {
        Self {
            prefixes: source.effective_ignore_startswith(),
            data_skip: source.effective_ignore(),
        }
    }

    fn excluded(&self, schema: &str, name: &str) -> bool {
        let qualified = format!("{schema}.{name}");
        self.prefixes.iter().any(|p| {
            schema.starts_with(p.as_str())
                || name.starts_with(p.as_str())
                || qualified.starts_with(p.as_str())
        })
    }

    fn data_skipped(&self, schema: &str, name: &str) -> bool {
        let qualified = format!("{schema}.{name}");
        self.data_skip.iter().any(|n| *n == qualified || *n == name)
    }
}

impl Plan {
    fn build(source: &Source, catalog: &Catalog) -> Self {
        let rules = Rules::from_source(source);
        let mut plan = Self {
            items: Vec::new(),
            index: HashMap::new(),
        };
        plan.add_schemas(catalog, &rules);
        plan.add_extensions(catalog);
        plan.add_types(catalog, &rules);
        plan.add_sequences(catalog, &rules);
        plan.add_relations(catalog, &rules);
        plan.add_functions(catalog, &rules);
        plan.add_triggers(catalog, &rules);
        plan
    }

    fn push(&mut self, item: Item, keys: &[(&'static str, u32)]) {
        let idx = self.items.len();
        for key in keys {
            self.index.insert(*key, idx);
        }
        self.items.push(item);
    }

    fn add_schemas(&mut self, catalog: &Catalog, rules: &Rules) {
        for (i, s) in catalog.schemas.iter().enumerate() {
            if rules.excluded(&s.name, "") {
                continue;
            }
            let item = Item {
                name: s.name.clone(),
                kind: ObjectKind::Schema,
                stem: sanitize(&s.name),
                what: What::Schema(i),
                data_skipped: false,
            };
            self.push(item, &[("pg_namespace", s.oid)]);
        }
    }

    fn add_extensions(&mut self, catalog: &Catalog) {
        for (i, e) in catalog.extensions.iter().enumerate() {
            let item = Item {
                name: format!("extension:{}", e.name),
                kind: ObjectKind::Extension,
                stem: sanitize(&format!("extension.{}", e.name)),
                what: What::Extension(i),
                data_skipped: false,
            };
            self.push(item, &[("pg_extension", e.oid)]);
        }
    }

    fn add_types(&mut self, catalog: &Catalog, rules: &Rules) {
        for (i, t) in catalog.types.iter().enumerate() {
            if rules.excluded(&t.schema, &t.name) {
                continue;
            }
            let name = format!("{}.{}", t.schema, t.name);
            let item = Item {
                stem: sanitize(&name),
                name,
                kind: ObjectKind::Type,
                what: What::Type(i),
                data_skipped: false,
            };
            self.push(item, &[("pg_type", t.oid), ("pg_class", t.relid)]);
        }
    }

    /// Identity sequences are created by their column and restored with the
    /// table; every other sequence is its own object.
    fn add_sequences(&mut self, catalog: &Catalog, rules: &Rules) {
        for (i, s) in catalog.sequences.iter().enumerate() {
            let identity = s.owner_dep.as_deref() == Some("i");
            if identity || rules.excluded(&s.schema, &s.name) {
                continue;
            }
            let name = format!("{}.{}", s.schema, s.name);
            let item = Item {
                stem: sanitize(&name),
                name,
                kind: ObjectKind::Sequence,
                what: What::Sequence(i),
                data_skipped: false,
            };
            self.push(item, &[("pg_class", s.oid)]);
        }
    }

    fn add_relations(&mut self, catalog: &Catalog, rules: &Rules) {
        for (i, r) in catalog.relations.iter().enumerate() {
            if rules.excluded(&r.schema, &r.name) {
                continue;
            }
            let kind = match r.kind {
                RelKind::Table | RelKind::Partitioned => ObjectKind::Table,
                RelKind::View => ObjectKind::View,
                RelKind::Matview => ObjectKind::Matview,
            };
            let name = format!("{}.{}", r.schema, r.name);
            let item = Item {
                stem: sanitize(&name),
                data_skipped: kind == ObjectKind::Table && rules.data_skipped(&r.schema, &r.name),
                name,
                kind,
                what: What::Relation(i),
            };
            self.push(item, &[("pg_class", r.oid)]);
        }
    }

    fn add_functions(&mut self, catalog: &Catalog, rules: &Rules) {
        for (i, f) in catalog.functions.iter().enumerate() {
            if f.def.is_none() || rules.excluded(&f.schema, &f.name) {
                continue;
            }
            let name = format!("{}.{}({})", f.schema, f.name, f.args);
            let item = Item {
                stem: sanitize(&name),
                name,
                kind: ObjectKind::Function,
                what: What::Function(i),
                data_skipped: false,
            };
            self.push(item, &[("pg_proc", f.oid)]);
        }
    }

    /// Triggers inherited from a partitioned parent are created with it.
    fn add_triggers(&mut self, catalog: &Catalog, rules: &Rules) {
        for (i, t) in catalog.triggers.iter().enumerate() {
            let Some(rel) = catalog.relations.iter().find(|r| r.oid == t.relid) else {
                continue;
            };
            if t.parent != 0 || rules.excluded(&rel.schema, &rel.name) {
                continue;
            }
            let name = format!("{}.{}.{}", rel.schema, rel.name, t.name);
            let item = Item {
                stem: sanitize(&name),
                name,
                kind: ObjectKind::Trigger,
                what: What::Trigger(i),
                data_skipped: false,
            };
            self.push(item, &[("pg_trigger", t.oid)]);
        }
    }

    /// Names this item must be created after, resolved from `pg_depend`
    /// (KB §5.5): FK parents, base relations, referenced types and
    /// functions, the owning schema. Self-references and a sequence's
    /// "owned by" link are not dependencies.
    fn depends_on(&self, idx: usize, catalog: &Catalog) -> Vec<String> {
        let mut deps = BTreeSet::new();
        if let What::Extension(i) = self.items[idx].what {
            let schema = &catalog.extensions[i].schema;
            deps.extend(
                self.items
                    .iter()
                    .filter(|it| it.kind == ObjectKind::Schema && it.name == *schema)
                    .map(|it| it.name.clone()),
            );
        }
        let is_sequence = self.items[idx].kind == ObjectKind::Sequence;
        for dep in &catalog.deps {
            if self.resolve_obj(dep.class.as_str(), dep.objid, catalog) != Some(idx) {
                continue;
            }
            let Some(target) = self.resolve_ref(dep.refclass.as_str(), dep.refobjid, catalog)
            else {
                continue;
            };
            let target_kind = self.items[target].kind;
            let owned_by = is_sequence && target_kind == ObjectKind::Table;
            if target == idx || owned_by {
                continue;
            }
            deps.insert(self.items[target].name.clone());
        }
        deps.into_iter().collect()
    }

    /// The dumped object an edge starts from: indexes, defaults, rules and
    /// constraints belong to their relation.
    fn resolve_obj(&self, class: &str, oid: u32, catalog: &Catalog) -> Option<usize> {
        let maps = &catalog.dep_maps;
        match class {
            "pg_rewrite" => self.lookup("pg_class", maps.rewrite_rel.get(&oid).copied()?),
            "pg_attrdef" => self.lookup("pg_class", maps.attrdef_rel.get(&oid).copied()?),
            "pg_constraint" => self.lookup("pg_class", maps.constraint_rel.get(&oid).copied()?),
            "pg_class" => self
                .lookup("pg_class", oid)
                .or_else(|| self.lookup("pg_class", maps.index_rel.get(&oid).copied()?)),
            "pg_type" | "pg_proc" | "pg_trigger" => self.lookup(class, oid),
            _ => None,
        }
    }

    /// The dumped object an edge points at. Anything an extension owns
    /// (its types, functions, operator classes, …) resolves to the extension.
    fn resolve_ref(&self, class: &str, oid: u32, catalog: &Catalog) -> Option<usize> {
        let maps = &catalog.dep_maps;
        if let Some(found) = self.lookup(class, oid) {
            return Some(found);
        }
        if let Some(ext) = maps.ext_owned.get(&(class.to_string(), oid)) {
            return self.lookup("pg_extension", *ext);
        }
        if class != "pg_type" {
            return None;
        }
        let (relid, elem) = maps.type_rel_elem.get(&oid).copied()?;
        if relid != 0 {
            return self.lookup("pg_class", relid);
        }
        if elem == 0 {
            return None;
        }
        self.lookup("pg_type", elem).or_else(|| {
            let ext = maps.ext_owned.get(&("pg_type".to_string(), elem))?;
            self.lookup("pg_extension", *ext)
        })
    }

    fn lookup(&self, class: &str, oid: u32) -> Option<usize> {
        const CLASSES: [&str; 6] = [
            "pg_class",
            "pg_type",
            "pg_proc",
            "pg_namespace",
            "pg_trigger",
            "pg_extension",
        ];
        let class = CLASSES.iter().find(|c| **c == class)?;
        self.index.get(&(*class, oid)).copied()
    }
}

/// A file-system-safe, deterministic stem for an object name. Names that
/// are already plain lowercase are used as they are; anything else is
/// mapped to a safe subset and suffixed with a hash of the original so two
/// names differing only in case or punctuation never share a file.
fn sanitize(name: &str) -> String {
    let plain = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.');
    if plain && !name.contains("..") && !name.starts_with('.') && name.len() <= 120 {
        return name.to_string();
    }
    let mapped: String = name
        .chars()
        .take(80)
        .map(|c| match c {
            'a'..='z' | '0'..='9' | '_' => c,
            'A'..='Z' => c.to_ascii_lowercase(),
            _ => '_',
        })
        .collect();
    let digest = sha256_hex(name.as_bytes());
    format!("{mapped}-{}", digest.get(..8).unwrap_or_default())
}

fn script_for(item: &Item, emitter: &Emitter<'_>) -> Script {
    let c = emitter.catalog;
    match item.what {
        What::Schema(i) => emitter.schema(&c.schemas[i]),
        What::Extension(i) => emitter.extension(&c.extensions[i]),
        What::Type(i) => emitter.type_def(&c.types[i]),
        What::Sequence(i) => emitter.sequence(&c.sequences[i]),
        What::Relation(i) if item.kind == ObjectKind::Table => emitter.table(&c.relations[i]),
        What::Relation(i) => emitter.view(&c.relations[i]),
        What::Function(i) => emitter.function(&c.functions[i]),
        What::Trigger(i) => emitter.trigger(&c.triggers[i]),
    }
}

/// Write one object's files and describe it for the manifest.
async fn write_object(
    conn: &Conn,
    source: &Source,
    dir: &Path,
    item: &Item,
    emitter: &Emitter<'_>,
    depends_on: Vec<String>,
) -> Result<ObjectEntry> {
    let script = script_for(item, emitter);
    let mut files = Vec::new();
    if source.effective_structure() {
        let path = format!("{}.schema.sql", item.stem);
        files.push(write_text(
            dir,
            &path,
            FileRole::Structure,
            &script.render(),
        )?);
        if let Some(post) = post_data(item, emitter) {
            let path = format!("{}.post.sql", item.stem);
            files.push(write_text(dir, &path, FileRole::PostData, &post.render())?);
        }
    }
    let (row_count, content_hash) = if wants_data(source, item, emitter.catalog) {
        let What::Relation(i) = item.what else {
            return Err(ArkError::Internal("table item without a relation".into()));
        };
        let rel = &emitter.catalog.relations[i];
        let path = format!("{}.data.copy", item.stem);
        let (entry, sum) = copy_table(conn, dir, &path, &qualified(&rel.schema, &rel.name)).await?;
        files.push(entry);
        (Some(sum.rows()), Some(sum.finish()))
    } else {
        (None, None)
    };
    debug!(object = %item.name, kind = ?item.kind, files = files.len(), rows = row_count, "dumped");
    Ok(ObjectEntry {
        name: item.name.clone(),
        kind: item.kind,
        depends_on,
        row_count,
        content_hash,
        schema_hash: script.schema_hash(),
        consistency: Consistency::Snapshot,
        files,
    })
}

fn post_data(item: &Item, emitter: &Emitter<'_>) -> Option<Script> {
    let What::Relation(i) = item.what else {
        return None;
    };
    let script = emitter.post_data(&emitter.catalog.relations[i]);
    (!script.is_empty()).then_some(script)
}

/// Rows are dumped for plain tables and partitions; a partitioned parent
/// holds no rows of its own (`COPY` refuses it), and views have no data.
fn wants_data(source: &Source, item: &Item, catalog: &Catalog) -> bool {
    if !source.data || item.kind != ObjectKind::Table || item.data_skipped {
        return false;
    }
    match item.what {
        What::Relation(i) => catalog.relations[i].kind == RelKind::Table,
        _ => false,
    }
}

fn write_text(dir: &Path, name: &str, role: FileRole, text: &str) -> Result<FileEntry> {
    let path = dir.join(name);
    std::fs::write(&path, text)?;
    file_entry(&path, name, role)
}

fn file_entry(path: &Path, name: &str, role: FileRole) -> Result<FileEntry> {
    let (size, sha256) = digest_file(path)?;
    Ok(FileEntry {
        path: name.to_string(),
        role,
        size,
        sha256,
    })
}

/// Stream `COPY … TO STDOUT` (text) into `name`, folding every row line
/// into the content hash as it passes (KB §11.3).
async fn copy_table(
    conn: &Conn,
    dir: &Path,
    name: &str,
    table: &str,
) -> Result<(FileEntry, Sum256)> {
    let path: PathBuf = dir.join(name);
    let stream = conn
        .copy_out(&format!("COPY {table} TO STDOUT (FORMAT text)"))
        .await?;
    let mut stream = std::pin::pin!(stream);
    let mut out = BufWriter::new(tokio::fs::File::create(&path).await?);
    let mut sum = Sum256::new();
    let mut pending: Vec<u8> = Vec::new();
    while let Some(chunk) = stream
        .try_next()
        .await
        .map_err(|e| super::conn::engine_err("COPY stream failed", e))?
    {
        out.write_all(&chunk).await?;
        fold_lines(&chunk, &mut pending, &mut sum);
    }
    if !pending.is_empty() {
        sum.add_row(&pending);
    }
    out.flush().await?;
    drop(out);
    Ok((file_entry(&path, name, FileRole::Data)?, sum))
}

/// Split `chunk` on newlines, completing any partial line carried in
/// `pending`; every completed line (without its newline) is one row.
fn fold_lines(chunk: &[u8], pending: &mut Vec<u8>, sum: &mut Sum256) {
    let mut rest = chunk;
    while let Some(pos) = rest.iter().position(|b| *b == b'\n') {
        let (line, tail) = rest.split_at(pos);
        if pending.is_empty() {
            sum.add_row(line);
        } else {
            pending.extend_from_slice(line);
            sum.add_row(pending);
            pending.clear();
        }
        rest = tail.get(1..).unwrap_or_default();
    }
    pending.extend_from_slice(rest);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_plain_names_and_hashes_the_rest() {
        assert_eq!(sanitize("public.orders"), "public.orders");
        let mixed = sanitize("public.Orders");
        assert!(mixed.starts_with("public_orders-"), "{mixed}");
        assert_ne!(mixed, sanitize("public.orders"));
        let spaced = sanitize("public.my table");
        assert!(spaced.starts_with("public_my_table-"), "{spaced}");
        assert!(!sanitize("a..b").contains(".."));
        assert!(!sanitize("../x").starts_with('.'));
    }

    #[test]
    fn fold_lines_hashes_rows_across_chunk_boundaries() {
        let mut whole = Sum256::new();
        whole.add_row(b"1\talpha");
        whole.add_row(b"2\tbeta");
        let mut split = Sum256::new();
        let mut pending = Vec::new();
        fold_lines(b"1\tal", &mut pending, &mut split);
        fold_lines(b"pha\n2\tbe", &mut pending, &mut split);
        fold_lines(b"ta\n", &mut pending, &mut split);
        assert!(pending.is_empty());
        assert_eq!(split.rows(), 2);
        assert_eq!(split.finish(), whole.finish());
    }

    #[test]
    fn rules_apply_prefixes_outright_and_ignore_as_data_skip() {
        let rules = Rules {
            prefixes: vec!["pg_".into(), "audit.".into()],
            data_skip: vec!["public.sessions".into(), "events".into()],
        };
        assert!(rules.excluded("audit", "log"));
        assert!(rules.excluded("public", "pg_stat_copy"));
        assert!(!rules.excluded("public", "orders"));
        assert!(rules.data_skipped("public", "sessions"));
        assert!(rules.data_skipped("app", "events"));
        assert!(!rules.data_skipped("public", "orders"));
    }
}
