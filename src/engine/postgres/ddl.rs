//! Emit portable DDL from catalog rows (KB §11.2 fidelity contract). The
//! output is Arkstore's own text — no server-version-specific statements,
//! ownership and privileges only when asked for — so restore applies it as
//! is. Each script also yields the canonical text behind `schema_hash`:
//! definitions only, with ownership, privileges, and sequence values (which
//! are data, not structure) left out.

use std::collections::HashMap;

use super::catalog::{
    Catalog, Column, Extension, Function, Grant, Policy, RelKind, Relation, Schema, Sequence,
    Trigger, TypeDef, TypeKind,
};
use super::sql::{escape, qualified, quote};
use crate::hash::schema_hash;

/// Which part of the hash a statement belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Definition,
    Privilege,
    Value,
}

/// An ordered list of statements plus what each one is.
#[derive(Debug, Clone, Default)]
pub struct Script {
    statements: Vec<(Part, String)>,
}

impl Script {
    fn def(&mut self, text: String) {
        self.statements.push((Part::Definition, text));
    }

    fn privilege(&mut self, text: String) {
        self.statements.push((Part::Privilege, text));
    }

    fn value(&mut self, text: String) {
        self.statements.push((Part::Value, text));
    }

    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    /// The file contents: one statement per line group, `;`-terminated.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for (_, text) in &self.statements {
            out.push_str(text);
            out.push_str(";\n");
        }
        out
    }

    /// `sha256:` over the definition statements with whitespace collapsed.
    pub fn schema_hash(&self) -> String {
        let canonical: Vec<String> = self
            .statements
            .iter()
            .filter(|(part, _)| *part == Part::Definition)
            .map(|(_, text)| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        schema_hash(&canonical.join("\n"))
    }
}

/// Emission settings and the lookups every emitter needs.
pub struct Emitter<'a> {
    pub catalog: &'a Catalog,
    pub include_privileges: bool,
    /// Relation oid → quoted `"schema"."name"`.
    pub rel_names: HashMap<u32, String>,
}

impl Emitter<'_> {
    fn rel_name(&self, oid: u32) -> String {
        self.rel_names
            .get(&oid)
            .cloned()
            .unwrap_or_else(|| format!("<unknown relation {oid}>"))
    }

    fn comment(&self, script: &mut Script, kind: &str, target: &str, comment: &Option<String>) {
        if let Some(text) = comment {
            script.def(format!("COMMENT ON {kind} {target} IS {}", escape(text)));
        }
    }

    fn owner(&self, script: &mut Script, kind: &str, target: &str, owner: &str) {
        if self.include_privileges {
            script.privilege(format!("ALTER {kind} {target} OWNER TO {}", quote(owner)));
        }
    }

    fn grants(&self, script: &mut Script, class: &str, oid: u32, kind: &str, target: &str) {
        if !self.include_privileges {
            return;
        }
        let key = (class.to_string(), oid);
        for grant in self.catalog.acls.get(&key).into_iter().flatten() {
            script.privilege(grant_statement(grant, kind, target));
        }
    }

    pub fn schema(&self, schema: &Schema) -> Script {
        let mut script = Script::default();
        let name = quote(&schema.name);
        script.def(format!("CREATE SCHEMA IF NOT EXISTS {name}"));
        self.comment(&mut script, "SCHEMA", &name, &schema.comment);
        self.owner(&mut script, "SCHEMA", &name, &schema.owner);
        self.grants(&mut script, "namespace", schema.oid, "SCHEMA", &name);
        script
    }

    pub fn extension(&self, ext: &Extension) -> Script {
        let mut script = Script::default();
        script.def(format!(
            "CREATE EXTENSION IF NOT EXISTS {} WITH SCHEMA {}",
            quote(&ext.name),
            quote(&ext.schema)
        ));
        script
    }

    pub fn type_def(&self, t: &TypeDef) -> Script {
        let mut script = Script::default();
        let name = qualified(&t.schema, &t.name);
        let kind = if t.kind == TypeKind::Domain {
            "DOMAIN"
        } else {
            "TYPE"
        };
        match t.kind {
            TypeKind::Enum => script.def(format!(
                "CREATE TYPE {name} AS ENUM ({})",
                t.enum_labels
                    .iter()
                    .map(|l| escape(l))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            TypeKind::Composite => script.def(format!(
                "CREATE TYPE {name} AS (\n{}\n)",
                self.composite_attributes(t.relid)
            )),
            TypeKind::Domain => script.def(domain_definition(&name, t)),
            TypeKind::Range => script.def(range_definition(&name, t)),
        }
        self.comment(&mut script, kind, &name, &t.comment);
        self.owner(&mut script, kind, &name, &t.owner);
        script
    }

    fn composite_attributes(&self, relid: u32) -> String {
        self.catalog
            .columns
            .get(&relid)
            .into_iter()
            .flatten()
            .map(|c| {
                let collate = c
                    .collation
                    .as_ref()
                    .map(|x| format!(" COLLATE {x}"))
                    .unwrap_or_default();
                format!("    {} {}{collate}", quote(&c.name), c.data_type)
            })
            .collect::<Vec<_>>()
            .join(",\n")
    }

    /// A standalone or `serial`-owned sequence: definition plus its value.
    pub fn sequence(&self, s: &Sequence) -> Script {
        let mut script = Script::default();
        let name = qualified(&s.schema, &s.name);
        let cycle = if s.cycle { "CYCLE" } else { "NO CYCLE" };
        script.def(format!(
            "CREATE SEQUENCE {name} AS {} INCREMENT BY {} MINVALUE {} MAXVALUE {} START WITH {} CACHE {} {cycle}",
            s.data_type, s.increment, s.min, s.max, s.start, s.cache
        ));
        self.comment(&mut script, "SEQUENCE", &name, &s.comment);
        self.owner(&mut script, "SEQUENCE", &name, &s.owner);
        self.grants(&mut script, "class", s.oid, "SEQUENCE", &name);
        push_setval(&mut script, &name, s.last_value);
        script
    }

    /// A table's structure file: definition, local constraints (not FKs),
    /// indexes, owned sequences, row security, comments.
    pub fn table(&self, rel: &Relation) -> Script {
        let mut script = Script::default();
        let name = self.rel_name(rel.oid);
        script.def(self.create_table(rel, &name));
        for c in self.catalog.constraints.get(&rel.oid).into_iter().flatten() {
            if c.contype == "f" {
                continue;
            }
            script.def(format!(
                "ALTER TABLE {name} ADD CONSTRAINT {} {}",
                quote(&c.name),
                c.def
            ));
            let target = format!("{} ON {name}", quote(&c.name));
            self.comment(&mut script, "CONSTRAINT", &target, &c.comment);
        }
        self.indexes(&mut script, rel);
        self.owned_sequences(&mut script, rel, &name);
        self.row_security(&mut script, rel, &name);
        self.comment(&mut script, "TABLE", &name, &rel.comment);
        self.column_comments_into(&mut script, rel.oid, &name);
        self.owner(&mut script, "TABLE", &name, &rel.owner);
        self.grants(&mut script, "class", rel.oid, "TABLE", &name);
        script
    }

    /// What must wait until every object's data is loaded: a table's
    /// foreign keys, a materialized view's refresh.
    pub fn post_data(&self, rel: &Relation) -> Script {
        let mut script = Script::default();
        let name = self.rel_name(rel.oid);
        if rel.kind == RelKind::Matview && rel.populated {
            script.value(format!("REFRESH MATERIALIZED VIEW {name}"));
        }
        for c in self.catalog.constraints.get(&rel.oid).into_iter().flatten() {
            if c.contype != "f" {
                continue;
            }
            script.def(format!(
                "ALTER TABLE {name} ADD CONSTRAINT {} {}",
                quote(&c.name),
                c.def
            ));
            let target = format!("{} ON {name}", quote(&c.name));
            self.comment(&mut script, "CONSTRAINT", &target, &c.comment);
        }
        script
    }

    fn create_table(&self, rel: &Relation, name: &str) -> String {
        let unlogged = if rel.unlogged { "UNLOGGED " } else { "" };
        let mut text = if rel.is_partition {
            let parent = self.parents(rel).first().cloned().unwrap_or_default();
            let bound = rel.part_bound.as_deref().unwrap_or("DEFAULT");
            let bound = if bound == "DEFAULT" {
                "DEFAULT".to_string()
            } else {
                bound.to_string()
            };
            format!("CREATE {unlogged}TABLE {name} PARTITION OF {parent} {bound}")
        } else {
            let parents = self.parents(rel);
            let inherits = if parents.is_empty() {
                String::new()
            } else {
                format!(" INHERITS ({})", parents.join(", "))
            };
            format!(
                "CREATE {unlogged}TABLE {name} (\n{}\n){inherits}",
                self.column_definitions(rel)
            )
        };
        if let Some(key) = &rel.part_key {
            text.push_str(&format!(" PARTITION BY {key}"));
        }
        if let Some(am) = &rel.access_method {
            text.push_str(&format!(" USING {}", quote(am)));
        }
        if let Some(options) = &rel.options {
            text.push_str(&format!(" WITH ({options})"));
        }
        text
    }

    fn parents(&self, rel: &Relation) -> Vec<String> {
        self.catalog
            .inherits
            .get(&rel.oid)
            .into_iter()
            .flatten()
            .map(|p| self.rel_name(*p))
            .collect()
    }

    fn column_definitions(&self, rel: &Relation) -> String {
        let is_child = self.catalog.inherits.contains_key(&rel.oid);
        self.catalog
            .columns
            .get(&rel.oid)
            .into_iter()
            .flatten()
            .filter(|c| !is_child || c.is_local)
            .map(|c| format!("    {}", self.column_definition(c)))
            .collect::<Vec<_>>()
            .join(",\n")
    }

    fn column_definition(&self, c: &Column) -> String {
        let mut text = format!("{} {}", quote(&c.name), c.data_type);
        if let Some(collation) = &c.collation {
            text.push_str(&format!(" COLLATE {collation}"));
        }
        if c.not_null {
            text.push_str(" NOT NULL");
        }
        match (c.generated.as_str(), c.default_expr.as_deref()) {
            ("s", Some(expr)) => text.push_str(&format!(" GENERATED ALWAYS AS ({expr}) STORED")),
            (_, Some(expr)) => text.push_str(&format!(" DEFAULT {expr}")),
            _ => {}
        }
        if let Some(identity) = self.identity_clause(c) {
            text.push_str(&identity);
        }
        text
    }

    fn identity_clause(&self, c: &Column) -> Option<String> {
        let when = match c.identity.as_str() {
            "a" => "ALWAYS",
            "d" => "BY DEFAULT",
            _ => return None,
        };
        let seq = self.catalog.sequences.iter().find(|s| {
            s.owner_dep.as_deref() == Some("i")
                && s.owner_rel == Some(c.relid)
                && s.owner_col == Some(c.num)
        });
        let options = seq
            .map(|s| {
                let cycle = if s.cycle { "CYCLE" } else { "NO CYCLE" };
                format!(
                    " (SEQUENCE NAME {} START WITH {} INCREMENT BY {} MINVALUE {} MAXVALUE {} CACHE {} {cycle})",
                    qualified(&s.schema, &s.name),
                    s.start,
                    s.increment,
                    s.min,
                    s.max,
                    s.cache
                )
            })
            .unwrap_or_default();
        Some(format!(" GENERATED {when} AS IDENTITY{options}"))
    }

    fn indexes(&self, script: &mut Script, rel: &Relation) {
        for index in self.catalog.indexes.get(&rel.oid).into_iter().flatten() {
            let def = if rel.kind == RelKind::Partitioned {
                index.def.replacen(" ON ONLY ", " ON ", 1)
            } else {
                index.def.clone()
            };
            script.def(def);
            let target = qualified(&rel.schema, &index.name);
            self.comment(script, "INDEX", &target, &index.comment);
        }
    }

    /// `serial` sequences become owned by their column; identity sequences
    /// (created by the column itself) only need their value restored.
    fn owned_sequences(&self, script: &mut Script, rel: &Relation, name: &str) {
        let columns = self.catalog.columns.get(&rel.oid);
        for s in self
            .catalog
            .sequences
            .iter()
            .filter(|s| s.owner_rel == Some(rel.oid))
        {
            let seq = qualified(&s.schema, &s.name);
            let column = columns
                .into_iter()
                .flatten()
                .find(|c| Some(c.num) == s.owner_col)
                .map(|c| quote(&c.name));
            match (s.owner_dep.as_deref(), column) {
                (Some("a"), Some(column)) => {
                    script.def(format!("ALTER SEQUENCE {seq} OWNED BY {name}.{column}"));
                }
                (Some("i"), _) => push_setval(script, &seq, s.last_value),
                _ => {}
            }
        }
    }

    fn row_security(&self, script: &mut Script, rel: &Relation, name: &str) {
        if rel.row_security {
            script.def(format!("ALTER TABLE {name} ENABLE ROW LEVEL SECURITY"));
        }
        if rel.force_row_security {
            script.def(format!("ALTER TABLE {name} FORCE ROW LEVEL SECURITY"));
        }
        for p in self.catalog.policies.get(&rel.oid).into_iter().flatten() {
            script.def(policy_definition(p, name));
        }
    }

    fn column_comments(&self, relid: u32, script_target: &str) -> Vec<String> {
        self.catalog
            .columns
            .get(&relid)
            .into_iter()
            .flatten()
            .filter_map(|c| {
                c.comment.as_ref().map(|text| {
                    format!(
                        "COMMENT ON COLUMN {script_target}.{} IS {}",
                        quote(&c.name),
                        escape(text)
                    )
                })
            })
            .collect()
    }

    pub fn view(&self, rel: &Relation) -> Script {
        let mut script = Script::default();
        let name = self.rel_name(rel.oid);
        let def = rel.view_def.as_deref().unwrap_or_default();
        let def = def.trim().trim_end_matches(';');
        let options = rel
            .options
            .as_ref()
            .map(|o| format!(" WITH ({o})"))
            .unwrap_or_default();
        let kind = if rel.kind == RelKind::Matview {
            let am = rel
                .access_method
                .as_ref()
                .map(|am| format!(" USING {}", quote(am)))
                .unwrap_or_default();
            script.def(format!(
                "CREATE MATERIALIZED VIEW {name}{am}{options} AS\n{def}\nWITH NO DATA"
            ));
            self.indexes(&mut script, rel);
            "MATERIALIZED VIEW"
        } else {
            script.def(format!("CREATE VIEW {name}{options} AS\n{def}"));
            "VIEW"
        };
        self.comment(&mut script, kind, &name, &rel.comment);
        self.column_comments_into(&mut script, rel.oid, &name);
        self.owner(&mut script, kind, &name, &rel.owner);
        self.grants(&mut script, "class", rel.oid, "TABLE", &name);
        script
    }

    pub fn function(&self, f: &Function) -> Script {
        let mut script = Script::default();
        let Some(def) = &f.def else {
            return script;
        };
        script.def(def.trim().trim_end_matches(';').to_string());
        let kind = if f.kind == "p" {
            "PROCEDURE"
        } else {
            "FUNCTION"
        };
        let target = format!("{}({})", qualified(&f.schema, &f.name), f.args);
        self.comment(&mut script, kind, &target, &f.comment);
        self.owner(&mut script, kind, &target, &f.owner);
        self.grants(&mut script, "proc", f.oid, kind, &target);
        script
    }

    pub fn trigger(&self, t: &Trigger) -> Script {
        let mut script = Script::default();
        let table = self.rel_name(t.relid);
        script.def(t.def.trim().trim_end_matches(';').to_string());
        let state = match t.enabled.as_str() {
            "D" => Some("DISABLE TRIGGER"),
            "R" => Some("ENABLE REPLICA TRIGGER"),
            "A" => Some("ENABLE ALWAYS TRIGGER"),
            _ => None,
        };
        if let Some(state) = state {
            script.def(format!("ALTER TABLE {table} {state} {}", quote(&t.name)));
        }
        let target = format!("{} ON {table}", quote(&t.name));
        self.comment(&mut script, "TRIGGER", &target, &t.comment);
        script
    }
}

impl Emitter<'_> {
    fn column_comments_into(&self, script: &mut Script, relid: u32, target: &str) {
        for statement in self.column_comments(relid, target) {
            script.def(statement);
        }
    }
}

fn push_setval(script: &mut Script, name: &str, last_value: Option<i64>) {
    if let Some(value) = last_value {
        script.value(format!(
            "SELECT pg_catalog.setval({}, {value}, true)",
            escape(name)
        ));
    }
}

fn domain_definition(name: &str, t: &TypeDef) -> String {
    let mut text = format!(
        "CREATE DOMAIN {name} AS {}",
        t.domain_base.as_deref().unwrap_or("text")
    );
    if let Some(collation) = &t.domain_collation {
        text.push_str(&format!(" COLLATE {collation}"));
    }
    if let Some(default) = &t.domain_default {
        text.push_str(&format!(" DEFAULT {default}"));
    }
    if t.domain_not_null {
        text.push_str(" NOT NULL");
    }
    for constraint in &t.domain_constraints {
        text.push_str(&format!(" {constraint}"));
    }
    text
}

fn range_definition(name: &str, t: &TypeDef) -> String {
    let Some(range) = &t.range else {
        return format!("CREATE TYPE {name} AS RANGE (SUBTYPE = text)");
    };
    let mut parts = vec![format!("SUBTYPE = {}", range.subtype)];
    if let Some(opclass) = &range.opclass {
        parts.push(format!("SUBTYPE_OPCLASS = {opclass}"));
    }
    if let Some(collation) = &range.collation {
        parts.push(format!("COLLATION = {collation}"));
    }
    if let Some(canonical) = &range.canonical {
        parts.push(format!("CANONICAL = {canonical}"));
    }
    if let Some(diff) = &range.subtype_diff {
        parts.push(format!("SUBTYPE_DIFF = {diff}"));
    }
    format!("CREATE TYPE {name} AS RANGE ({})", parts.join(", "))
}

fn policy_definition(p: &Policy, table: &str) -> String {
    let kind = if p.permissive {
        "PERMISSIVE"
    } else {
        "RESTRICTIVE"
    };
    let command = match p.cmd.as_str() {
        "r" => "SELECT",
        "a" => "INSERT",
        "w" => "UPDATE",
        "d" => "DELETE",
        _ => "ALL",
    };
    let roles = if p.roles.is_empty() {
        "PUBLIC".to_string()
    } else {
        p.roles
            .iter()
            .map(|r| quote(r))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut text = format!(
        "CREATE POLICY {} ON {table} AS {kind} FOR {command} TO {roles}",
        quote(&p.name)
    );
    if let Some(using) = &p.using {
        text.push_str(&format!(" USING ({using})"));
    }
    if let Some(check) = &p.check {
        text.push_str(&format!(" WITH CHECK ({check})"));
    }
    text
}

fn grant_statement(grant: &Grant, kind: &str, target: &str) -> String {
    let grantee = if grant.grantee == "PUBLIC" {
        "PUBLIC".to_string()
    } else {
        quote(&grant.grantee)
    };
    let option = if grant.grantable {
        " WITH GRANT OPTION"
    } else {
        ""
    };
    format!(
        "GRANT {} ON {kind} {target} TO {grantee}{option}",
        grant.privilege
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relation(oid: u32, name: &str, kind: RelKind) -> Relation {
        Relation {
            oid,
            schema: "public".into(),
            name: name.into(),
            kind,
            unlogged: false,
            is_partition: false,
            part_bound: None,
            part_key: None,
            options: None,
            row_security: false,
            force_row_security: false,
            comment: None,
            owner: "app".into(),
            view_def: None,
            populated: true,
            access_method: None,
        }
    }

    fn column(relid: u32, num: i32, name: &str, data_type: &str) -> Column {
        Column {
            relid,
            num,
            name: name.into(),
            data_type: data_type.into(),
            not_null: false,
            default_expr: None,
            identity: String::new(),
            generated: String::new(),
            collation: None,
            comment: None,
            is_local: true,
        }
    }

    fn emitter(catalog: &Catalog, include_privileges: bool) -> Emitter<'_> {
        let rel_names = catalog
            .relations
            .iter()
            .map(|r| (r.oid, qualified(&r.schema, &r.name)))
            .collect();
        Emitter {
            catalog,
            include_privileges,
            rel_names,
        }
    }

    #[test]
    fn table_ddl_covers_columns_constraints_and_keeps_fks_separate() {
        let mut catalog = Catalog::default();
        let mut orders = relation(10, "orders", RelKind::Table);
        orders.comment = Some("all orders".into());
        catalog.relations.push(orders);
        let mut id = column(10, 1, "id", "integer");
        id.not_null = true;
        id.identity = "d".into();
        let mut total = column(10, 2, "total", "numeric(10,2)");
        total.default_expr = Some("0".into());
        let mut doubled = column(10, 3, "doubled", "numeric");
        doubled.generated = "s".into();
        doubled.default_expr = Some("(total * 2)".into());
        catalog.columns.insert(10, vec![id, total, doubled]);
        catalog.constraints.insert(
            10,
            vec![
                super::super::catalog::Constraint {
                    relid: 10,
                    name: "orders_pkey".into(),
                    contype: "p".into(),
                    def: "PRIMARY KEY (id)".into(),
                    comment: None,
                },
                super::super::catalog::Constraint {
                    relid: 10,
                    name: "orders_customer_fkey".into(),
                    contype: "f".into(),
                    def: "FOREIGN KEY (id) REFERENCES public.customers(id)".into(),
                    comment: None,
                },
            ],
        );
        let e = emitter(&catalog, false);
        let rel = &catalog.relations[0];
        let structure = e.table(rel).render();
        assert!(structure.contains("CREATE TABLE \"public\".\"orders\" (\n    \"id\" integer NOT NULL GENERATED BY DEFAULT AS IDENTITY,\n    \"total\" numeric(10,2) DEFAULT 0,\n    \"doubled\" numeric GENERATED ALWAYS AS ((total * 2)) STORED\n);"), "{structure}");
        assert!(structure.contains("ADD CONSTRAINT \"orders_pkey\" PRIMARY KEY (id);"));
        assert!(structure.contains("COMMENT ON TABLE \"public\".\"orders\" IS 'all orders';"));
        assert!(!structure.contains("FOREIGN KEY"), "{structure}");
        assert!(!structure.contains("OWNER TO"));
        let fks = e.post_data(rel).render();
        assert!(fks.contains("ADD CONSTRAINT \"orders_customer_fkey\" FOREIGN KEY"));
    }

    #[test]
    fn privileges_are_opt_in_and_excluded_from_the_schema_hash() {
        let mut catalog = Catalog::default();
        catalog.relations.push(relation(10, "t", RelKind::Table));
        catalog.columns.insert(10, vec![column(10, 1, "x", "text")]);
        catalog.acls.insert(
            ("class".into(), 10),
            vec![Grant {
                grantee: "reader".into(),
                privilege: "SELECT".into(),
                grantable: false,
            }],
        );
        let without = emitter(&catalog, false).table(&catalog.relations[0]);
        let with = emitter(&catalog, true).table(&catalog.relations[0]);
        assert!(!without.render().contains("GRANT"));
        assert!(with
            .render()
            .contains("GRANT SELECT ON TABLE \"public\".\"t\" TO \"reader\";"));
        assert!(with
            .render()
            .contains("ALTER TABLE \"public\".\"t\" OWNER TO \"app\";"));
        assert_eq!(with.schema_hash(), without.schema_hash());
    }

    #[test]
    fn sequence_values_are_emitted_but_not_hashed() {
        let seq = Sequence {
            oid: 20,
            schema: "public".into(),
            name: "s".into(),
            data_type: "bigint".into(),
            start: 1,
            increment: 1,
            min: 1,
            max: 9_223_372_036_854_775_807,
            cache: 1,
            cycle: false,
            last_value: Some(42),
            owner_rel: None,
            owner_col: None,
            owner_dep: None,
            comment: None,
            owner: "app".into(),
        };
        let catalog = Catalog::default();
        let e = emitter(&catalog, false);
        let script = e.sequence(&seq);
        let text = script.render();
        assert!(
            text.contains("CREATE SEQUENCE \"public\".\"s\" AS bigint INCREMENT BY 1 MINVALUE 1")
        );
        assert!(text.contains("SELECT pg_catalog.setval('\"public\".\"s\"', 42, true);"));
        let mut unset = seq.clone();
        unset.last_value = None;
        assert_eq!(e.sequence(&unset).schema_hash(), script.schema_hash());
    }

    #[test]
    fn views_and_partitioned_indexes_render_portably() {
        let mut catalog = Catalog::default();
        let mut v = relation(30, "v", RelKind::View);
        v.view_def = Some(" SELECT 1;".into());
        let mut parent = relation(31, "events", RelKind::Partitioned);
        parent.part_key = Some("RANGE (at)".into());
        let mut child = relation(32, "events_2026", RelKind::Table);
        child.is_partition = true;
        child.part_bound = Some("FOR VALUES FROM ('2026-01-01') TO ('2027-01-01')".into());
        catalog.relations.extend([v, parent, child]);
        catalog
            .columns
            .insert(31, vec![column(31, 1, "at", "date")]);
        catalog.inherits.insert(32, vec![31]);
        catalog.indexes.insert(
            31,
            vec![super::super::catalog::Index {
                relid: 31,
                name: "events_at_idx".into(),
                def: "CREATE INDEX events_at_idx ON ONLY public.events USING btree (at)".into(),
                comment: None,
            }],
        );
        let e = emitter(&catalog, false);
        assert_eq!(
            e.view(&catalog.relations[0]).render(),
            "CREATE VIEW \"public\".\"v\" AS\nSELECT 1;\n"
        );
        let parent_ddl = e.table(&catalog.relations[1]).render();
        assert!(
            parent_ddl.contains(") PARTITION BY RANGE (at);"),
            "{parent_ddl}"
        );
        assert!(
            parent_ddl.contains("ON public.events USING btree"),
            "{parent_ddl}"
        );
        let child_ddl = e.table(&catalog.relations[2]).render();
        assert_eq!(
            child_ddl,
            "CREATE TABLE \"public\".\"events_2026\" PARTITION OF \"public\".\"events\" FOR VALUES FROM ('2026-01-01') TO ('2027-01-01');\n"
        );
    }

    #[test]
    fn policies_and_types_render() {
        let p = Policy {
            relid: 1,
            name: "own_rows".into(),
            permissive: true,
            cmd: "r".into(),
            roles: vec![],
            using: Some("(owner = current_user)".into()),
            check: None,
        };
        assert_eq!(
            policy_definition(&p, "\"public\".\"t\""),
            "CREATE POLICY \"own_rows\" ON \"public\".\"t\" AS PERMISSIVE FOR SELECT TO PUBLIC USING ((owner = current_user))"
        );
        let t = TypeDef {
            oid: 1,
            schema: "public".into(),
            name: "mood".into(),
            kind: TypeKind::Enum,
            comment: None,
            owner: "app".into(),
            enum_labels: vec!["sad".into(), "it's ok".into()],
            domain_base: None,
            domain_not_null: false,
            domain_default: None,
            domain_constraints: vec![],
            domain_collation: None,
            range: None,
            relid: 0,
        };
        let catalog = Catalog::default();
        let e = emitter(&catalog, false);
        assert_eq!(
            e.type_def(&t).render(),
            "CREATE TYPE \"public\".\"mood\" AS ENUM ('sad', 'it''s ok');\n"
        );
    }
}
