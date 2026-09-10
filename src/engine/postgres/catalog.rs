//! Read the schema from `pg_catalog` inside the snapshot (KB §11.2). Every
//! query is version-gated at PostgreSQL 13 by the connection, uses only
//! catalogs and `pg_get_*` helpers present since then, and excludes objects
//! owned by extensions (they come back with `CREATE EXTENSION`).

use std::collections::HashMap;

use tokio_postgres::types::FromSql;
use tokio_postgres::Row;

use super::conn::{engine_err, Conn};
use crate::error::Result;

/// The user-schema filter shared by every query (`n` is `pg_namespace`).
const USER_SCHEMA: &str = "n.nspname NOT IN ('pg_catalog', 'information_schema', 'pg_toast') \
    AND n.nspname NOT LIKE 'pg\\_temp\\_%' AND n.nspname NOT LIKE 'pg\\_toast\\_temp\\_%'";

/// First OID a user-created object can have.
const FIRST_USER_OID: u32 = 16_384;

#[derive(Debug, Clone)]
pub struct Schema {
    pub oid: u32,
    pub name: String,
    pub comment: Option<String>,
    pub owner: String,
}

#[derive(Debug, Clone)]
pub struct Extension {
    pub oid: u32,
    pub name: String,
    pub schema: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    Enum,
    Composite,
    Domain,
    Range,
}

#[derive(Debug, Clone)]
pub struct RangeDef {
    pub subtype: String,
    pub opclass: Option<String>,
    pub collation: Option<String>,
    pub canonical: Option<String>,
    pub subtype_diff: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TypeDef {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub kind: TypeKind,
    pub comment: Option<String>,
    pub owner: String,
    pub enum_labels: Vec<String>,
    pub domain_base: Option<String>,
    pub domain_not_null: bool,
    pub domain_default: Option<String>,
    /// `CONSTRAINT name CHECK (…)` fragments.
    pub domain_constraints: Vec<String>,
    pub domain_collation: Option<String>,
    pub range: Option<RangeDef>,
    /// The row-type relation of a composite type (its columns live there).
    pub relid: u32,
}

#[derive(Debug, Clone)]
pub struct Sequence {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub data_type: String,
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    pub cache: i64,
    pub cycle: bool,
    pub last_value: Option<i64>,
    /// Owning table / column when the sequence backs a `serial` (`a`) or an
    /// identity column (`i`).
    pub owner_rel: Option<u32>,
    pub owner_col: Option<i32>,
    pub owner_dep: Option<String>,
    pub comment: Option<String>,
    pub owner: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelKind {
    Table,
    Partitioned,
    View,
    Matview,
}

#[derive(Debug, Clone)]
pub struct Relation {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub kind: RelKind,
    pub unlogged: bool,
    pub is_partition: bool,
    pub part_bound: Option<String>,
    pub part_key: Option<String>,
    pub options: Option<String>,
    pub row_security: bool,
    pub force_row_security: bool,
    pub comment: Option<String>,
    pub owner: String,
    pub view_def: Option<String>,
    pub populated: bool,
    pub access_method: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub relid: u32,
    pub num: i32,
    pub name: String,
    pub data_type: String,
    pub not_null: bool,
    pub default_expr: Option<String>,
    /// `a` (always) / `d` (by default) / empty.
    pub identity: String,
    /// `s` (stored) / empty.
    pub generated: String,
    pub collation: Option<String>,
    pub comment: Option<String>,
    pub is_local: bool,
}

#[derive(Debug, Clone)]
pub struct Constraint {
    pub relid: u32,
    pub name: String,
    /// `p` / `u` / `c` / `x` / `f`.
    pub contype: String,
    pub def: String,
    pub comment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Index {
    pub relid: u32,
    pub name: String,
    pub def: String,
    pub comment: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Trigger {
    pub oid: u32,
    pub relid: u32,
    pub name: String,
    pub def: String,
    /// `O` enabled, `D` disabled, `R` replica, `A` always.
    pub enabled: String,
    pub comment: Option<String>,
    pub parent: u32,
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub relid: u32,
    pub name: String,
    pub permissive: bool,
    pub cmd: String,
    pub roles: Vec<String>,
    pub using: Option<String>,
    pub check: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub oid: u32,
    pub schema: String,
    pub name: String,
    pub args: String,
    /// `f` function, `p` procedure, `w` window, `a` aggregate.
    pub kind: String,
    pub def: Option<String>,
    pub comment: Option<String>,
    pub owner: String,
}

/// One `pg_depend` edge between user objects.
#[derive(Debug, Clone)]
pub struct Dep {
    pub class: String,
    pub objid: u32,
    pub refclass: String,
    pub refobjid: u32,
}

#[derive(Debug, Clone)]
pub struct Grant {
    pub grantee: String,
    pub privilege: String,
    pub grantable: bool,
}

/// Helper lookups for resolving `pg_depend` edges to objects.
#[derive(Debug, Clone, Default)]
pub struct DepMaps {
    /// `pg_rewrite` oid → view relation.
    pub rewrite_rel: HashMap<u32, u32>,
    /// `pg_attrdef` oid → table relation.
    pub attrdef_rel: HashMap<u32, u32>,
    /// `pg_constraint` oid → table relation.
    pub constraint_rel: HashMap<u32, u32>,
    /// `pg_type` oid → (row-type relation, element type).
    pub type_rel_elem: HashMap<u32, (u32, u32)>,
    /// index oid → indexed relation.
    pub index_rel: HashMap<u32, u32>,
    /// (catalog, oid) of an extension-owned object → the extension.
    pub ext_owned: HashMap<(String, u32), u32>,
}

/// Everything the dump needs, read in one snapshot.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub schemas: Vec<Schema>,
    pub extensions: Vec<Extension>,
    pub types: Vec<TypeDef>,
    pub sequences: Vec<Sequence>,
    pub relations: Vec<Relation>,
    pub columns: HashMap<u32, Vec<Column>>,
    pub constraints: HashMap<u32, Vec<Constraint>>,
    pub indexes: HashMap<u32, Vec<Index>>,
    pub triggers: Vec<Trigger>,
    pub policies: HashMap<u32, Vec<Policy>>,
    /// child relation → parents in `inhseqno` order.
    pub inherits: HashMap<u32, Vec<u32>>,
    pub functions: Vec<Function>,
    pub deps: Vec<Dep>,
    pub dep_maps: DepMaps,
    /// (`class` | `namespace` | `proc`, oid) → grants.
    pub acls: HashMap<(String, u32), Vec<Grant>>,
    /// Object classes outside the fidelity contract, with counts.
    pub unsupported: Vec<(String, i64)>,
}

fn col<'a, T: FromSql<'a>>(row: &'a Row, idx: usize) -> Result<T> {
    row.try_get(idx)
        .map_err(|e| engine_err(&format!("catalog column {idx}"), e))
}

fn not_ext(class: &str, oid_expr: &str) -> String {
    format!(
        "NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend x WHERE x.classid = 'pg_catalog.{class}'::regclass \
         AND x.objid = {oid_expr} AND x.deptype = 'e')"
    )
}

impl Catalog {
    /// Read every catalog the fidelity contract covers.
    pub async fn load(conn: &Conn, include_privileges: bool) -> Result<Self> {
        let mut catalog = Self {
            schemas: load_schemas(conn).await?,
            extensions: load_extensions(conn).await?,
            types: load_types(conn).await?,
            sequences: load_sequences(conn).await?,
            relations: load_relations(conn).await?,
            columns: group(load_columns(conn).await?, |c| c.relid),
            constraints: group(load_constraints(conn).await?, |c| c.relid),
            indexes: group(load_indexes(conn).await?, |i| i.relid),
            triggers: load_triggers(conn).await?,
            policies: group(load_policies(conn).await?, |p| p.relid),
            inherits: load_inherits(conn).await?,
            functions: load_functions(conn).await?,
            deps: load_deps(conn).await?,
            dep_maps: load_dep_maps(conn).await?,
            acls: HashMap::new(),
            unsupported: load_unsupported(conn).await?,
        };
        if include_privileges {
            catalog.acls = load_acls(conn).await?;
        }
        Ok(catalog)
    }
}

fn group<T, K: std::hash::Hash + Eq>(items: Vec<T>, key: impl Fn(&T) -> K) -> HashMap<K, Vec<T>> {
    let mut out: HashMap<K, Vec<T>> = HashMap::new();
    for item in items {
        out.entry(key(&item)).or_default().push(item);
    }
    out
}

async fn load_schemas(conn: &Conn) -> Result<Vec<Schema>> {
    let sql = format!(
        "SELECT n.oid, n.nspname::text, pg_catalog.obj_description(n.oid, 'pg_namespace'), \
         pg_catalog.pg_get_userbyid(n.nspowner)::text \
         FROM pg_catalog.pg_namespace n WHERE {USER_SCHEMA} AND {} ORDER BY n.nspname",
        not_ext("pg_namespace", "n.oid")
    );
    conn.rows(&sql, &[])
        .await?
        .iter()
        .map(|r| {
            Ok(Schema {
                oid: col(r, 0)?,
                name: col(r, 1)?,
                comment: col(r, 2)?,
                owner: col(r, 3)?,
            })
        })
        .collect()
}

async fn load_extensions(conn: &Conn) -> Result<Vec<Extension>> {
    let sql = "SELECT e.oid, e.extname::text, n.nspname::text FROM pg_catalog.pg_extension e \
               JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace \
               WHERE e.extname <> 'plpgsql' ORDER BY e.extname";
    conn.rows(sql, &[])
        .await?
        .iter()
        .map(|r| {
            Ok(Extension {
                oid: col(r, 0)?,
                name: col(r, 1)?,
                schema: col(r, 2)?,
            })
        })
        .collect()
}

const TYPES_SQL: &str = "SELECT t.oid, n.nspname::text, t.typname::text, t.typtype::text, \
    pg_catalog.obj_description(t.oid, 'pg_type'), pg_catalog.pg_get_userbyid(t.typowner)::text, \
    COALESCE((SELECT pg_catalog.array_agg(e.enumlabel::text ORDER BY e.enumsortorder) \
              FROM pg_catalog.pg_enum e WHERE e.enumtypid = t.oid), '{}'::text[]), \
    CASE WHEN t.typtype = 'd' THEN pg_catalog.format_type(t.typbasetype, t.typtypmod) END, \
    t.typnotnull, t.typdefault, \
    COALESCE((SELECT pg_catalog.array_agg('CONSTRAINT ' || pg_catalog.quote_ident(c.conname) || ' ' \
              || pg_catalog.pg_get_constraintdef(c.oid, true) ORDER BY c.conname) \
              FROM pg_catalog.pg_constraint c WHERE c.contypid = t.oid), '{}'::text[]), \
    CASE WHEN t.typtype = 'd' AND t.typcollation <> 0 AND t.typcollation <> bt.typcollation \
         THEN pg_catalog.quote_ident(cn.nspname) || '.' || pg_catalog.quote_ident(co.collname) END, \
    CASE WHEN t.typtype = 'r' THEN pg_catalog.format_type(r.rngsubtype, NULL) END, \
    CASE WHEN t.typtype = 'r' AND r.rngsubopc <> 0 \
         THEN pg_catalog.quote_ident(opn.nspname) || '.' || pg_catalog.quote_ident(opc.opcname) END, \
    CASE WHEN t.typtype = 'r' AND r.rngcollation <> 0 \
         THEN pg_catalog.quote_ident(rcn.nspname) || '.' || pg_catalog.quote_ident(rco.collname) END, \
    CASE WHEN t.typtype = 'r' AND r.rngcanonical <> 0 THEN r.rngcanonical::regproc::text END, \
    CASE WHEN t.typtype = 'r' AND r.rngsubdiff <> 0 THEN r.rngsubdiff::regproc::text END, \
    t.typrelid \
    FROM pg_catalog.pg_type t \
    JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
    LEFT JOIN pg_catalog.pg_type bt ON bt.oid = t.typbasetype \
    LEFT JOIN pg_catalog.pg_collation co ON co.oid = t.typcollation \
    LEFT JOIN pg_catalog.pg_namespace cn ON cn.oid = co.collnamespace \
    LEFT JOIN pg_catalog.pg_range r ON r.rngtypid = t.oid \
    LEFT JOIN pg_catalog.pg_opclass opc ON opc.oid = r.rngsubopc \
    LEFT JOIN pg_catalog.pg_namespace opn ON opn.oid = opc.opcnamespace \
    LEFT JOIN pg_catalog.pg_collation rco ON rco.oid = r.rngcollation \
    LEFT JOIN pg_catalog.pg_namespace rcn ON rcn.oid = rco.collnamespace \
    LEFT JOIN pg_catalog.pg_class rc ON rc.oid = t.typrelid \
    WHERE (t.typtype IN ('e', 'd', 'r') OR (t.typtype = 'c' AND rc.relkind = 'c')) AND ";

async fn load_types(conn: &Conn) -> Result<Vec<TypeDef>> {
    let sql = format!(
        "{TYPES_SQL}{USER_SCHEMA} AND {} ORDER BY n.nspname, t.typname",
        not_ext("pg_type", "t.oid")
    );
    conn.rows(&sql, &[])
        .await?
        .iter()
        .map(type_from_row)
        .collect()
}

fn type_from_row(r: &Row) -> Result<TypeDef> {
    let typtype: String = col(r, 3)?;
    let kind = match typtype.as_str() {
        "e" => TypeKind::Enum,
        "d" => TypeKind::Domain,
        "r" => TypeKind::Range,
        _ => TypeKind::Composite,
    };
    let range = match kind {
        TypeKind::Range => Some(RangeDef {
            subtype: col::<Option<String>>(r, 12)?.unwrap_or_default(),
            opclass: col(r, 13)?,
            collation: col(r, 14)?,
            canonical: col(r, 15)?,
            subtype_diff: col(r, 16)?,
        }),
        _ => None,
    };
    Ok(TypeDef {
        oid: col(r, 0)?,
        schema: col(r, 1)?,
        name: col(r, 2)?,
        kind,
        comment: col(r, 4)?,
        owner: col(r, 5)?,
        enum_labels: col(r, 6)?,
        domain_base: col(r, 7)?,
        domain_not_null: col(r, 8)?,
        domain_default: col(r, 9)?,
        domain_constraints: col(r, 10)?,
        domain_collation: col(r, 11)?,
        range,
        relid: col(r, 17)?,
    })
}

async fn load_sequences(conn: &Conn) -> Result<Vec<Sequence>> {
    let sql = format!(
        "SELECT c.oid, n.nspname::text, c.relname::text, pg_catalog.format_type(s.seqtypid, NULL), \
         s.seqstart, s.seqincrement, s.seqmin, s.seqmax, s.seqcache, s.seqcycle, \
         (SELECT ps.last_value FROM pg_catalog.pg_sequences ps \
          WHERE ps.schemaname = n.nspname AND ps.sequencename = c.relname), \
         dep.refobjid, dep.refobjsubid, dep.deptype::text, \
         pg_catalog.obj_description(c.oid, 'pg_class'), pg_catalog.pg_get_userbyid(c.relowner)::text \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_catalog.pg_sequence s ON s.seqrelid = c.oid \
         LEFT JOIN pg_catalog.pg_depend dep ON dep.classid = 'pg_catalog.pg_class'::regclass \
              AND dep.objid = c.oid AND dep.refclassid = 'pg_catalog.pg_class'::regclass \
              AND dep.deptype IN ('a', 'i') \
         WHERE c.relkind = 'S' AND {USER_SCHEMA} AND {} ORDER BY n.nspname, c.relname",
        not_ext("pg_class", "c.oid")
    );
    conn.rows(&sql, &[])
        .await?
        .iter()
        .map(|r| {
            Ok(Sequence {
                oid: col(r, 0)?,
                schema: col(r, 1)?,
                name: col(r, 2)?,
                data_type: col(r, 3)?,
                start: col(r, 4)?,
                increment: col(r, 5)?,
                min: col(r, 6)?,
                max: col(r, 7)?,
                cache: col(r, 8)?,
                cycle: col(r, 9)?,
                last_value: col(r, 10)?,
                owner_rel: col(r, 11)?,
                owner_col: col(r, 12)?,
                owner_dep: col(r, 13)?,
                comment: col(r, 14)?,
                owner: col(r, 15)?,
            })
        })
        .collect()
}

async fn load_relations(conn: &Conn) -> Result<Vec<Relation>> {
    let sql = format!(
        "SELECT c.oid, n.nspname::text, c.relname::text, c.relkind::text, c.relpersistence::text, \
         c.relispartition, \
         CASE WHEN c.relispartition THEN pg_catalog.pg_get_expr(c.relpartbound, c.oid, true) END, \
         CASE WHEN c.relkind = 'p' THEN pg_catalog.pg_get_partkeydef(c.oid) END, \
         pg_catalog.array_to_string(c.reloptions, ', '), c.relrowsecurity, c.relforcerowsecurity, \
         pg_catalog.obj_description(c.oid, 'pg_class'), pg_catalog.pg_get_userbyid(c.relowner)::text, \
         CASE WHEN c.relkind IN ('v', 'm') THEN pg_catalog.pg_get_viewdef(c.oid, true) END, \
         c.relispopulated, \
         CASE WHEN c.relkind IN ('r', 'm') AND am.amname <> 'heap' THEN am.amname::text END \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam \
         WHERE c.relkind IN ('r', 'p', 'v', 'm') AND {USER_SCHEMA} AND {} \
         ORDER BY n.nspname, c.relname",
        not_ext("pg_class", "c.oid")
    );
    conn.rows(&sql, &[])
        .await?
        .iter()
        .map(relation_from_row)
        .collect()
}

fn relation_from_row(r: &Row) -> Result<Relation> {
    let relkind: String = col(r, 3)?;
    let persistence: String = col(r, 4)?;
    Ok(Relation {
        oid: col(r, 0)?,
        schema: col(r, 1)?,
        name: col(r, 2)?,
        kind: match relkind.as_str() {
            "p" => RelKind::Partitioned,
            "v" => RelKind::View,
            "m" => RelKind::Matview,
            _ => RelKind::Table,
        },
        unlogged: persistence == "u",
        is_partition: col(r, 5)?,
        part_bound: col(r, 6)?,
        part_key: col(r, 7)?,
        options: col::<Option<String>>(r, 8)?.filter(|o| !o.is_empty()),
        row_security: col(r, 9)?,
        force_row_security: col(r, 10)?,
        comment: col(r, 11)?,
        owner: col(r, 12)?,
        view_def: col(r, 13)?,
        populated: col(r, 14)?,
        access_method: col(r, 15)?,
    })
}

async fn load_columns(conn: &Conn) -> Result<Vec<Column>> {
    let sql = format!(
        "SELECT a.attrelid, a.attnum::int, a.attname::text, \
         pg_catalog.format_type(a.atttypid, a.atttypmod), a.attnotnull, \
         pg_catalog.pg_get_expr(d.adbin, d.adrelid, true), a.attidentity::text, a.attgenerated::text, \
         CASE WHEN a.attcollation <> 0 AND a.attcollation <> t.typcollation \
              THEN pg_catalog.quote_ident(cn.nspname) || '.' || pg_catalog.quote_ident(co.collname) END, \
         pg_catalog.col_description(a.attrelid, a.attnum), a.attislocal \
         FROM pg_catalog.pg_attribute a \
         JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_catalog.pg_type t ON t.oid = a.atttypid \
         LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
         LEFT JOIN pg_catalog.pg_collation co ON co.oid = a.attcollation \
         LEFT JOIN pg_catalog.pg_namespace cn ON cn.oid = co.collnamespace \
         WHERE a.attnum > 0 AND NOT a.attisdropped AND c.relkind IN ('r', 'p', 'c', 'v', 'm') \
         AND {USER_SCHEMA} ORDER BY a.attrelid, a.attnum"
    );
    conn.rows(&sql, &[])
        .await?
        .iter()
        .map(|r| {
            Ok(Column {
                relid: col(r, 0)?,
                num: col(r, 1)?,
                name: col(r, 2)?,
                data_type: col(r, 3)?,
                not_null: col(r, 4)?,
                default_expr: col(r, 5)?,
                identity: col(r, 6)?,
                generated: col(r, 7)?,
                collation: col(r, 8)?,
                comment: col(r, 9)?,
                is_local: col(r, 10)?,
            })
        })
        .collect()
}

async fn load_constraints(conn: &Conn) -> Result<Vec<Constraint>> {
    let sql = format!(
        "SELECT c.conrelid, c.conname::text, c.contype::text, \
         pg_catalog.pg_get_constraintdef(c.oid, true), \
         pg_catalog.obj_description(c.oid, 'pg_constraint') \
         FROM pg_catalog.pg_constraint c \
         JOIN pg_catalog.pg_class r ON r.oid = c.conrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = r.relnamespace \
         WHERE c.contype IN ('p', 'u', 'c', 'x', 'f') AND c.conislocal AND {USER_SCHEMA} \
         ORDER BY c.conrelid, c.contype, c.conname"
    );
    conn.rows(&sql, &[])
        .await?
        .iter()
        .map(|r| {
            Ok(Constraint {
                relid: col(r, 0)?,
                name: col(r, 1)?,
                contype: col(r, 2)?,
                def: col(r, 3)?,
                comment: col(r, 4)?,
            })
        })
        .collect()
}

/// Indexes that are neither constraint-backed (those come with the
/// constraint) nor attached to a partitioned parent index (those are
/// created by the parent's `CREATE INDEX`).
async fn load_indexes(conn: &Conn) -> Result<Vec<Index>> {
    let sql = format!(
        "SELECT i.indrelid, ic.relname::text, pg_catalog.pg_get_indexdef(i.indexrelid), \
         pg_catalog.obj_description(i.indexrelid, 'pg_class') \
         FROM pg_catalog.pg_index i \
         JOIN pg_catalog.pg_class ic ON ic.oid = i.indexrelid \
         JOIN pg_catalog.pg_class tc ON tc.oid = i.indrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = tc.relnamespace \
         WHERE {USER_SCHEMA} AND {} \
         AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_constraint k \
                         WHERE k.conindid = i.indexrelid AND k.contype IN ('p', 'u', 'x')) \
         AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_inherits h WHERE h.inhrelid = i.indexrelid) \
         ORDER BY i.indrelid, ic.relname",
        not_ext("pg_class", "i.indexrelid")
    );
    conn.rows(&sql, &[])
        .await?
        .iter()
        .map(|r| {
            Ok(Index {
                relid: col(r, 0)?,
                name: col(r, 1)?,
                def: col(r, 2)?,
                comment: col(r, 3)?,
            })
        })
        .collect()
}

async fn load_triggers(conn: &Conn) -> Result<Vec<Trigger>> {
    let sql = format!(
        "SELECT t.oid, t.tgrelid, t.tgname::text, pg_catalog.pg_get_triggerdef(t.oid, true), \
         t.tgenabled::text, pg_catalog.obj_description(t.oid, 'pg_trigger'), t.tgparentid \
         FROM pg_catalog.pg_trigger t \
         JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE NOT t.tgisinternal AND {USER_SCHEMA} ORDER BY t.tgrelid, t.tgname"
    );
    conn.rows(&sql, &[])
        .await?
        .iter()
        .map(|r| {
            Ok(Trigger {
                oid: col(r, 0)?,
                relid: col(r, 1)?,
                name: col(r, 2)?,
                def: col(r, 3)?,
                enabled: col(r, 4)?,
                comment: col(r, 5)?,
                parent: col(r, 6)?,
            })
        })
        .collect()
}

async fn load_policies(conn: &Conn) -> Result<Vec<Policy>> {
    let sql = format!(
        "SELECT p.polrelid, p.polname::text, p.polpermissive, p.polcmd::text, \
         COALESCE((SELECT pg_catalog.array_agg(pg_catalog.pg_get_userbyid(r)::text) \
                   FROM pg_catalog.unnest(p.polroles) AS r WHERE r <> 0), '{{}}'::text[]), \
         pg_catalog.pg_get_expr(p.polqual, p.polrelid, true), \
         pg_catalog.pg_get_expr(p.polwithcheck, p.polrelid, true) \
         FROM pg_catalog.pg_policy p \
         JOIN pg_catalog.pg_class c ON c.oid = p.polrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE {USER_SCHEMA} ORDER BY p.polrelid, p.polname"
    );
    conn.rows(&sql, &[])
        .await?
        .iter()
        .map(|r| {
            Ok(Policy {
                relid: col(r, 0)?,
                name: col(r, 1)?,
                permissive: col(r, 2)?,
                cmd: col(r, 3)?,
                roles: col(r, 4)?,
                using: col(r, 5)?,
                check: col(r, 6)?,
            })
        })
        .collect()
}

async fn load_inherits(conn: &Conn) -> Result<HashMap<u32, Vec<u32>>> {
    let sql = format!(
        "SELECT i.inhrelid, i.inhparent FROM pg_catalog.pg_inherits i \
         JOIN pg_catalog.pg_class c ON c.oid = i.inhrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p') AND {USER_SCHEMA} ORDER BY i.inhrelid, i.inhseqno"
    );
    let mut out: HashMap<u32, Vec<u32>> = HashMap::new();
    for r in conn.rows(&sql, &[]).await? {
        let child: u32 = col(&r, 0)?;
        let parent: u32 = col(&r, 1)?;
        out.entry(child).or_default().push(parent);
    }
    Ok(out)
}

async fn load_functions(conn: &Conn) -> Result<Vec<Function>> {
    let sql = format!(
        "SELECT p.oid, n.nspname::text, p.proname::text, \
         pg_catalog.pg_get_function_identity_arguments(p.oid), p.prokind::text, \
         CASE WHEN p.prokind IN ('f', 'p', 'w') THEN pg_catalog.pg_get_functiondef(p.oid) END, \
         pg_catalog.obj_description(p.oid, 'pg_proc'), pg_catalog.pg_get_userbyid(p.proowner)::text \
         FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
         WHERE {USER_SCHEMA} AND {} \
         AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend i WHERE i.classid = 'pg_catalog.pg_proc'::regclass \
                         AND i.objid = p.oid AND i.deptype = 'i') \
         ORDER BY n.nspname, p.proname, 4",
        not_ext("pg_proc", "p.oid")
    );
    conn.rows(&sql, &[])
        .await?
        .iter()
        .map(|r| {
            Ok(Function {
                oid: col(r, 0)?,
                schema: col(r, 1)?,
                name: col(r, 2)?,
                args: col(r, 3)?,
                kind: col(r, 4)?,
                def: col(r, 5)?,
                comment: col(r, 6)?,
                owner: col(r, 7)?,
            })
        })
        .collect()
}

async fn load_deps(conn: &Conn) -> Result<Vec<Dep>> {
    let sql = format!(
        "SELECT d.classid::regclass::text, d.objid, d.refclassid::regclass::text, d.refobjid \
         FROM pg_catalog.pg_depend d \
         WHERE d.deptype IN ('n', 'a') AND d.refobjid >= {FIRST_USER_OID} \
         AND d.classid IN ('pg_catalog.pg_class'::regclass, 'pg_catalog.pg_type'::regclass, \
             'pg_catalog.pg_proc'::regclass, 'pg_catalog.pg_rewrite'::regclass, \
             'pg_catalog.pg_constraint'::regclass, 'pg_catalog.pg_attrdef'::regclass, \
             'pg_catalog.pg_trigger'::regclass)"
    );
    conn.rows(&sql, &[])
        .await?
        .iter()
        .map(|r| {
            Ok(Dep {
                class: col::<String>(r, 0)?
                    .rsplit('.')
                    .next()
                    .unwrap_or_default()
                    .to_string(),
                objid: col(r, 1)?,
                refclass: col::<String>(r, 2)?
                    .rsplit('.')
                    .next()
                    .unwrap_or_default()
                    .to_string(),
                refobjid: col(r, 3)?,
            })
        })
        .collect()
}

async fn load_dep_maps(conn: &Conn) -> Result<DepMaps> {
    let mut maps = DepMaps::default();
    let rewrite = format!(
        "SELECT r.oid, r.ev_class FROM pg_catalog.pg_rewrite r WHERE r.ev_class >= {FIRST_USER_OID}"
    );
    for r in conn.rows(&rewrite, &[]).await? {
        maps.rewrite_rel.insert(col(&r, 0)?, col(&r, 1)?);
    }
    let attrdef = format!(
        "SELECT d.oid, d.adrelid FROM pg_catalog.pg_attrdef d WHERE d.adrelid >= {FIRST_USER_OID}"
    );
    for r in conn.rows(&attrdef, &[]).await? {
        maps.attrdef_rel.insert(col(&r, 0)?, col(&r, 1)?);
    }
    let constraint = format!(
        "SELECT c.oid, c.conrelid FROM pg_catalog.pg_constraint c \
         WHERE c.oid >= {FIRST_USER_OID} AND c.conrelid <> 0"
    );
    for r in conn.rows(&constraint, &[]).await? {
        maps.constraint_rel.insert(col(&r, 0)?, col(&r, 1)?);
    }
    let types = format!(
        "SELECT t.oid, t.typrelid, t.typelem FROM pg_catalog.pg_type t WHERE t.oid >= {FIRST_USER_OID}"
    );
    for r in conn.rows(&types, &[]).await? {
        maps.type_rel_elem
            .insert(col(&r, 0)?, (col(&r, 1)?, col(&r, 2)?));
    }
    let indexes = format!(
        "SELECT i.indexrelid, i.indrelid FROM pg_catalog.pg_index i WHERE i.indrelid >= {FIRST_USER_OID}"
    );
    for r in conn.rows(&indexes, &[]).await? {
        maps.index_rel.insert(col(&r, 0)?, col(&r, 1)?);
    }
    let owned =
        "SELECT d.classid::regclass::text, d.objid, d.refobjid FROM pg_catalog.pg_depend d \
                 WHERE d.deptype = 'e' AND d.refclassid = 'pg_catalog.pg_extension'::regclass";
    for r in conn.rows(owned, &[]).await? {
        let class = col::<String>(&r, 0)?
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .to_string();
        maps.ext_owned.insert((class, col(&r, 1)?), col(&r, 2)?);
    }
    Ok(maps)
}

async fn load_unsupported(conn: &Conn) -> Result<Vec<(String, i64)>> {
    let sql = format!(
        "SELECT 'aggregate'::text, count(*) FROM pg_catalog.pg_proc p \
           JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
           WHERE p.prokind = 'a' AND {USER_SCHEMA} AND {ext_proc} \
         UNION ALL SELECT 'foreign table', count(*) FROM pg_catalog.pg_class c \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
           WHERE c.relkind = 'f' AND {USER_SCHEMA} \
         UNION ALL SELECT 'rule', count(*) FROM pg_catalog.pg_rewrite r \
           JOIN pg_catalog.pg_class c ON c.oid = r.ev_class \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
           WHERE r.rulename <> '_RETURN' AND c.relkind NOT IN ('v', 'm') AND {USER_SCHEMA} \
         UNION ALL SELECT 'base type', count(*) FROM pg_catalog.pg_type t \
           JOIN pg_catalog.pg_namespace n ON n.oid = t.typnamespace \
           WHERE t.typtype = 'b' AND t.typelem = 0 AND {USER_SCHEMA} AND {ext_type} \
         UNION ALL SELECT 'operator', count(*) FROM pg_catalog.pg_operator o \
           JOIN pg_catalog.pg_namespace n ON n.oid = o.oprnamespace \
           WHERE {USER_SCHEMA} AND {ext_op} \
         UNION ALL SELECT 'collation', count(*) FROM pg_catalog.pg_collation c \
           JOIN pg_catalog.pg_namespace n ON n.oid = c.collnamespace WHERE {USER_SCHEMA} \
         UNION ALL SELECT 'event trigger', count(*) FROM pg_catalog.pg_event_trigger \
         UNION ALL SELECT 'publication', count(*) FROM pg_catalog.pg_publication \
         UNION ALL SELECT 'large object', count(*) FROM pg_catalog.pg_largeobject_metadata",
        ext_proc = not_ext("pg_proc", "p.oid"),
        ext_type = not_ext("pg_type", "t.oid"),
        ext_op = not_ext("pg_operator", "o.oid"),
    );
    conn.rows(&sql, &[])
        .await?
        .iter()
        .map(|r| Ok((col(r, 0)?, col(r, 1)?)))
        .filter(|item| !matches!(item, Ok((_, 0))))
        .collect()
}

async fn load_acls(conn: &Conn) -> Result<HashMap<(String, u32), Vec<Grant>>> {
    let grantee = "CASE WHEN a.grantee = 0 THEN 'PUBLIC' \
                   ELSE pg_catalog.pg_get_userbyid(a.grantee)::text END";
    let sql = format!(
        "SELECT 'class'::text, c.oid, {grantee}, a.privilege_type::text, a.is_grantable \
           FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace, \
                pg_catalog.aclexplode(c.relacl) a \
           WHERE c.relkind IN ('r', 'p', 'v', 'm', 'S') AND {USER_SCHEMA} \
         UNION ALL SELECT 'namespace', n.oid, {grantee}, a.privilege_type::text, a.is_grantable \
           FROM pg_catalog.pg_namespace n, pg_catalog.aclexplode(n.nspacl) a WHERE {USER_SCHEMA} \
         UNION ALL SELECT 'proc', p.oid, {grantee}, a.privilege_type::text, a.is_grantable \
           FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace, \
                pg_catalog.aclexplode(p.proacl) a WHERE {USER_SCHEMA} \
         ORDER BY 1, 2, 3, 4"
    );
    let mut out: HashMap<(String, u32), Vec<Grant>> = HashMap::new();
    for r in conn.rows(&sql, &[]).await? {
        let key = (col::<String>(&r, 0)?, col::<u32>(&r, 1)?);
        out.entry(key).or_default().push(Grant {
            grantee: col(&r, 2)?,
            privilege: col(&r, 3)?,
            grantable: col(&r, 4)?,
        });
    }
    Ok(out)
}
