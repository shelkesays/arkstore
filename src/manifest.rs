//! `manifest.json` v1 — the archive's authority on what it contains
//! (KB §2.5) — plus the dependency-ordered load plan derived from it (§5.5).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::SourceType;
use crate::error::{ArkError, Result};

/// The manifest format version this build writes and reads.
pub const MANIFEST_VERSION: u32 = 1;

/// How the source snapshot was established.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// `pg_snapshot` | `mysql_consistent` | `mongo_none`.
    pub kind: String,
    /// The exported snapshot id (Postgres); `null` otherwise.
    #[serde(default)]
    pub id: Option<String>,
}

/// Whether an object was read inside the source snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Consistency {
    Snapshot,
    None,
    PerCollection,
}

/// What kind of database object an entry describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectKind {
    Table,
    View,
    Matview,
    Sequence,
    Function,
    Trigger,
    Type,
    Extension,
    Collection,
    MongoView,
}

/// The role a file plays for its object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileRole {
    Structure,
    Data,
    Metadata,
}

/// One file inside the archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Relative to the archive root; no `..`, no leading separator.
    pub path: String,
    pub role: FileRole,
    pub size: u64,
    /// Bare hex SHA-256 of the file bytes.
    pub sha256: String,
}

/// One dumped object.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectEntry {
    /// Schema-qualified (`schema.object`) or `db.collection`.
    pub name: String,
    pub kind: ObjectKind,
    /// Objects that must be created / loaded before this one.
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub row_count: Option<u64>,
    /// `sum256:<hex>`; SQL tables only.
    #[serde(default)]
    pub content_hash: Option<String>,
    /// `sha256:<hex>` of the canonicalised definition.
    pub schema_hash: String,
    pub consistency: Consistency,
    #[serde(default)]
    pub files: Vec<FileEntry>,
}

/// The manifest itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub manifest_version: u32,
    pub source: String,
    pub engine: SourceType,
    pub server_version: String,
    pub created_at: DateTime<Utc>,
    pub stamp: String,
    pub timezone: String,
    pub snapshot: Snapshot,
    /// `true` iff every object was read inside the source snapshot.
    pub consistent: bool,
    /// Session settings the data was encoded under (KB §11.3).
    #[serde(default)]
    pub session: BTreeMap<String, String>,
    #[serde(default)]
    pub objects: Vec<ObjectEntry>,
}

/// The dependency-ordered load plan (KB §5.5): layers to load in order, plus
/// any objects caught in a dependency cycle (loaded with constraints deferred).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadPlan<'a> {
    pub layers: Vec<Vec<&'a ObjectEntry>>,
    pub cyclic: Vec<&'a ObjectEntry>,
}

impl Manifest {
    /// Parse and validate a manifest from JSON bytes.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        let manifest: Manifest = serde_json::from_slice(bytes)?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Pretty JSON for writing into the archive.
    pub fn to_json(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(self)?)
    }

    /// Structural checks: version, unique names, safe unique paths, hash
    /// prefixes. Content checks (file presence / digests) happen at restore.
    pub fn validate(&self) -> Result<()> {
        if self.manifest_version != MANIFEST_VERSION {
            return Err(ArkError::Manifest(format!(
                "unsupported manifest_version {} (this build reads {MANIFEST_VERSION})",
                self.manifest_version
            )));
        }
        if self.source.trim().is_empty() {
            return Err(ArkError::Manifest("source is empty".into()));
        }
        let mut names = HashSet::new();
        let mut paths = HashSet::new();
        for object in &self.objects {
            validate_object(object, &mut names, &mut paths)?;
        }
        Ok(())
    }

    /// Every file path the manifest lists.
    pub fn file_paths(&self) -> impl Iterator<Item = &str> {
        self.objects
            .iter()
            .flat_map(|o| o.files.iter().map(|f| f.path.as_str()))
    }

    /// Topologically layer objects by `depends_on` (Kahn's algorithm).
    /// Dependencies on objects not in the manifest are ignored (the object may
    /// have been excluded by an ignore rule). Objects in a cycle are returned
    /// separately so the loader can defer their constraints.
    pub fn load_plan(&self) -> LoadPlan<'_> {
        let (mut indegree, dependents) = self.dependency_graph();
        let mut placed = vec![false; self.objects.len()];
        let mut frontier: VecDeque<usize> = (0..self.objects.len())
            .filter(|&i| indegree[i] == 0)
            .collect();
        let mut layers = Vec::new();
        while !frontier.is_empty() {
            layers.push(self.peel_layer(&mut frontier, &mut indegree, &dependents, &mut placed));
        }
        let cyclic = self
            .objects
            .iter()
            .enumerate()
            .filter(|(i, _)| !placed[*i])
            .map(|(_, o)| o)
            .collect();
        LoadPlan { layers, cyclic }
    }

    /// In-degree per object and, per object, the objects that depend on it.
    fn dependency_graph(&self) -> (Vec<usize>, Vec<Vec<usize>>) {
        let index: HashMap<&str, usize> = self
            .objects
            .iter()
            .enumerate()
            .map(|(i, o)| (o.name.as_str(), i))
            .collect();
        let n = self.objects.len();
        let mut indegree = vec![0usize; n];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
        for (i, object) in self.objects.iter().enumerate() {
            let deps = object
                .depends_on
                .iter()
                .filter_map(|d| index.get(d.as_str()).copied())
                .filter(|&d| d != i);
            for dep in deps {
                indegree[i] = indegree[i].saturating_add(1);
                dependents[dep].push(i);
            }
        }
        (indegree, dependents)
    }

    /// Drain the current frontier into one layer, releasing dependents whose
    /// in-degree drops to zero into the next frontier.
    fn peel_layer<'a>(
        &'a self,
        frontier: &mut VecDeque<usize>,
        indegree: &mut [usize],
        dependents: &[Vec<usize>],
        placed: &mut [bool],
    ) -> Vec<&'a ObjectEntry> {
        let mut layer = Vec::new();
        let mut next = VecDeque::new();
        while let Some(i) = frontier.pop_front() {
            placed[i] = true;
            layer.push(&self.objects[i]);
            for &j in &dependents[i] {
                release(j, indegree, &mut next);
            }
        }
        *frontier = next;
        layer
    }
}

fn release(j: usize, indegree: &mut [usize], next: &mut VecDeque<usize>) {
    indegree[j] = indegree[j].saturating_sub(1);
    if indegree[j] == 0 {
        next.push_back(j);
    }
}

fn validate_object<'a>(
    object: &'a ObjectEntry,
    names: &mut HashSet<&'a str>,
    paths: &mut HashSet<&'a str>,
) -> Result<()> {
    if object.name.trim().is_empty() {
        return Err(ArkError::Manifest("object with empty name".into()));
    }
    if !names.insert(object.name.as_str()) {
        return Err(ArkError::Manifest(format!(
            "duplicate object `{}`",
            object.name
        )));
    }
    if !object.schema_hash.starts_with("sha256:") {
        return Err(ArkError::Manifest(format!(
            "object `{}`: schema_hash must be `sha256:<hex>`",
            object.name
        )));
    }
    if object
        .content_hash
        .as_deref()
        .is_some_and(|h| !h.starts_with("sum256:"))
    {
        return Err(ArkError::Manifest(format!(
            "object `{}`: content_hash must be `sum256:<hex>`",
            object.name
        )));
    }
    for file in &object.files {
        validate_file(&object.name, file, paths)?;
    }
    Ok(())
}

fn validate_file<'a>(
    object: &str,
    file: &'a FileEntry,
    paths: &mut HashSet<&'a str>,
) -> Result<()> {
    check_relative_path(object, &file.path)?;
    if !paths.insert(file.path.as_str()) {
        return Err(ArkError::Manifest(format!(
            "duplicate file path `{}`",
            file.path
        )));
    }
    if file.sha256.len() != 64 || !file.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(ArkError::Manifest(format!(
            "file `{}`: sha256 must be 64 hex characters",
            file.path
        )));
    }
    Ok(())
}

fn check_relative_path(object: &str, path: &str) -> Result<()> {
    let bad = |why: &str| {
        Err(ArkError::Manifest(format!(
            "object `{object}`: file path `{path}` {why}"
        )))
    };
    if path.is_empty() {
        return bad("is empty");
    }
    if path.starts_with('/') || path.starts_with('\\') || path.contains(':') {
        return bad("must be relative");
    }
    if path.split(['/', '\\']).any(|seg| seg == "..") {
        return bad("must not contain `..`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(name: &str, deps: &[&str]) -> ObjectEntry {
        ObjectEntry {
            name: name.into(),
            kind: ObjectKind::Table,
            depends_on: deps.iter().map(|d| d.to_string()).collect(),
            row_count: Some(1),
            content_hash: Some(format!("sum256:{}", "0".repeat(64))),
            schema_hash: format!("sha256:{}", "0".repeat(64)),
            consistency: Consistency::Snapshot,
            files: vec![FileEntry {
                path: format!("{name}.schema.sql"),
                role: FileRole::Structure,
                size: 1,
                sha256: "0".repeat(64),
            }],
        }
    }

    fn manifest(objects: Vec<ObjectEntry>) -> Manifest {
        Manifest {
            manifest_version: MANIFEST_VERSION,
            source: "appdb".into(),
            engine: SourceType::Postgre,
            server_version: "16.3".into(),
            created_at: Utc::now(),
            stamp: "2026-09-04-074507".into(),
            timezone: "UTC".into(),
            snapshot: Snapshot {
                kind: "pg_snapshot".into(),
                id: Some("00000003-00000002-1".into()),
            },
            consistent: true,
            session: BTreeMap::new(),
            objects,
        }
    }

    #[test]
    fn json_roundtrip_preserves_everything() {
        let m = manifest(vec![
            object("public.customers", &[]),
            object("public.orders", &["public.customers"]),
        ]);
        let json = m.to_json().unwrap();
        assert!(json.contains("\"manifest_version\": 1"));
        assert!(json.contains("\"engine\": \"postgre\""));
        assert!(json.contains("\"consistency\": \"snapshot\""));
        let back = Manifest::from_json(json.as_bytes()).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn rejects_bad_version_paths_and_hashes() {
        let mut m = manifest(vec![object("a", &[])]);
        m.manifest_version = 2;
        assert!(m.validate().is_err());

        let mut m = manifest(vec![object("a", &[])]);
        m.objects[0].files[0].path = "../escape.sql".into();
        assert!(m.validate().is_err());
        m.objects[0].files[0].path = "/abs.sql".into();
        assert!(m.validate().is_err());

        let mut m = manifest(vec![object("a", &[]), object("a", &[])]);
        assert!(m.validate().is_err());
        m.objects[1].name = "b".into();
        m.objects[1].files[0].path = m.objects[0].files[0].path.clone();
        assert!(m.validate().is_err());

        let mut m = manifest(vec![object("a", &[])]);
        m.objects[0].schema_hash = "md5:x".into();
        assert!(m.validate().is_err());
    }

    #[test]
    fn load_plan_layers_parents_first_and_isolates_cycles() {
        let m = manifest(vec![
            object("orders", &["customers", "products"]),
            object("customers", &[]),
            object("products", &[]),
            object("audit", &["orders", "missing.ignored"]),
            object("x", &["y"]),
            object("y", &["x"]),
        ]);
        let plan = m.load_plan();
        fn names<'a>(layer: &[&'a ObjectEntry]) -> Vec<&'a str> {
            let mut v: Vec<&str> = layer.iter().map(|o| o.name.as_str()).collect();
            v.sort_unstable();
            v
        }
        assert_eq!(names(&plan.layers[0]), vec!["customers", "products"]);
        assert_eq!(names(&plan.layers[1]), vec!["orders"]);
        assert_eq!(names(&plan.layers[2]), vec!["audit"]);
        assert_eq!(plan.layers.len(), 3);
        assert_eq!(names(&plan.cyclic), vec!["x", "y"]);
    }
}
