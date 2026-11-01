//! Export du catalogue d'un projet en SQL : le schéma déclaratif obtenu en
//! rejouant des migrations (import brownfield, PLAN.md P2).
//!
//! L'ordre d'émission respecte les dépendances : schémas, extensions,
//! types, séquences autonomes, tables (contraintes hors clés étrangères),
//! routines et vues dans leur ordre de création, clés étrangères, index,
//! triggers, policies, commentaires. Seuls les objets du projet sont émis :
//! ni le catalogue intégré, ni les membres d'extensions.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

use crate::catalog::Catalog;
use crate::model::{
    ConstraintKind, IdentityKind, QName, Relation, RelationKind, TypeKind, TypeRef,
    quote_sql_ident as quote_ident,
};

/// Résultat de l'export.
pub struct Dump {
    /// Un fichier par schéma : `nom du schéma → SQL`.
    pub schemas: BTreeMap<String, String>,
    /// Migration de référence : tout le projet, en-tête `-- pgweave:` compris.
    pub baseline: String,
}

fn is_system(schema: &str) -> bool {
    schema == "pg_catalog" || schema == "information_schema" || schema == "pg_temp"
}

/// Nom d'objet en SQL : non qualifié dans `public`.
fn name_sql(q: &QName) -> String {
    if q.schema == "public" {
        quote_ident(&q.name)
    } else {
        format!("{}.{}", quote_ident(&q.schema), quote_ident(&q.name))
    }
}

/// Type en SQL : noms usuels pour les types intégrés.
pub fn type_sql(t: &TypeRef) -> String {
    match t {
        TypeRef::Array(inner) => format!("{}[]", type_sql(inner)),
        TypeRef::Named { name, modifiers } => {
            let mods = if modifiers.is_empty() {
                String::new()
            } else {
                format!("({})", modifiers.join(", "))
            };
            if name.schema == "pg_catalog" {
                let base = match name.name.as_str() {
                    "int2" => "smallint",
                    "int4" => "integer",
                    "int8" => "bigint",
                    "float4" => "real",
                    "float8" => "double precision",
                    "bool" => "boolean",
                    "bpchar" => "character",
                    "varchar" => "varchar",
                    "timestamptz" => return format!("timestamp{mods} with time zone"),
                    "timetz" => return format!("time{mods} with time zone"),
                    other => other,
                };
                format!("{base}{mods}")
            } else {
                format!("{}{mods}", name_sql(name))
            }
        }
        TypeRef::Unresolved(s) => s.clone(),
        TypeRef::Unknown => "unknown".into(),
    }
}

/// Phases d'émission, par schéma.
#[derive(Default)]
struct Out {
    phases: BTreeMap<(u8, String), Vec<(u64, String)>>,
}

impl Out {
    fn push(&mut self, phase: u8, schema: &str, order: u64, sql: String) {
        self.phases
            .entry((phase, schema.to_string()))
            .or_default()
            .push((order, sql));
    }
}

pub fn dump(cat: &Catalog, postgres: u32) -> Dump {
    let ext_types: BTreeSet<&QName> = cat
        .extension_packs
        .values()
        .flat_map(|p| p.types.iter().map(|t| &t.name))
        .collect();
    let ext_routines: BTreeSet<&QName> = cat
        .extension_packs
        .values()
        .flat_map(|p| p.routines.iter().map(|r| &r.name))
        .collect();

    let mut out = Out::default();
    let mut graph: Vec<Node> = Vec::new();

    // 1. Schémas.
    for s in cat.schemas.values() {
        if is_system(&s.name) || s.name == "public" {
            continue;
        }
        let owner = s
            .owner
            .as_ref()
            .map(|o| format!(" AUTHORIZATION {}", quote_ident(o)))
            .unwrap_or_default();
        out.push(
            1,
            &s.name,
            0,
            format!("CREATE SCHEMA {}{owner};", quote_ident(&s.name)),
        );
    }

    // 2. Extensions.
    for e in cat.extensions.values() {
        let version = e
            .version
            .as_ref()
            .map(|v| format!(" VERSION '{}'", v.replace('\'', "''")))
            .unwrap_or_default();
        out.push(
            2,
            &e.schema,
            0,
            format!(
                "CREATE EXTENSION IF NOT EXISTS {} SCHEMA {}{version};",
                quote_ident(&e.name),
                quote_ident(&e.schema)
            ),
        );
    }

    // 3. Types : énumérations et composites, puis domaines.
    for t in cat.types.values() {
        if is_system(&t.name.schema) || ext_types.contains(&t.name) {
            continue;
        }
        let (phase, sql) = match &t.kind {
            TypeKind::Enum { labels } => (
                3,
                format!(
                    "CREATE TYPE {} AS ENUM ({});",
                    name_sql(&t.name),
                    labels
                        .iter()
                        .map(|l| format!("'{}'", l.replace('\'', "''")))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
            TypeKind::Composite { attributes } => (
                3,
                format!(
                    "CREATE TYPE {} AS ({});",
                    name_sql(&t.name),
                    attributes
                        .iter()
                        .map(|a| format!("{} {}", quote_ident(&a.name), type_sql(&a.ty)))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ),
            TypeKind::Domain {
                base,
                not_null,
                default,
                checks,
            } => {
                let mut sql = format!("CREATE DOMAIN {} AS {}", name_sql(&t.name), type_sql(base));
                if let Some(d) = default {
                    let _ = write!(sql, " DEFAULT {d}");
                }
                if *not_null {
                    sql.push_str(" NOT NULL");
                }
                for c in checks {
                    if let Some(n) = &c.name {
                        let _ = write!(sql, " CONSTRAINT {}", quote_ident(n));
                    }
                    let _ = write!(sql, " CHECK ({})", c.expr);
                }
                sql.push(';');
                (4, sql)
            }
            // Types tableau : créés avec leur type.
            _ => continue,
        };
        out.push(phase, &t.name.schema, 0, sql);
    }

    // 4. Séquences créées explicitement.
    for s in cat.sequences.values() {
        if let Some(def) = &s.definition {
            out.push(5, &s.name.schema, 0, def.clone());
        }
    }

    // 5. Tables, puis 7. clés étrangères et contraintes NOT VALID.
    for r in cat.relations.values() {
        if is_system(&r.name.schema)
            || !matches!(r.kind, RelationKind::Table | RelationKind::PartitionedTable)
        {
            continue;
        }
        graph.push(Node {
            key: format!("r:{}", r.name),
            schema: r.name.schema.clone(),
            order: r.order,
            deps: table_deps(cat, r),
            sql: create_table(cat, r),
        });
        for c in &r.constraints {
            let deferred = matches!(c.kind, ConstraintKind::ForeignKey { .. }) || c.not_valid;
            if !deferred {
                continue;
            }
            // Après les index : une FK peut viser une colonne rendue unique
            // par un `CREATE UNIQUE INDEX`.
            out.push(
                10,
                &r.name.schema,
                r.order,
                format!(
                    "ALTER TABLE {} ADD CONSTRAINT {} {}{};",
                    name_sql(&r.name),
                    quote_ident(&c.name),
                    constraint_body(c),
                    if c.not_valid { " NOT VALID" } else { "" }
                ),
            );
        }
        if r.row_level_security {
            out.push(
                12,
                &r.name.schema,
                r.order,
                format!(
                    "ALTER TABLE {} ENABLE ROW LEVEL SECURITY;",
                    name_sql(&r.name)
                ),
            );
        }
    }

    // 6. Routines et vues, dans leur ordre de création (une fonction SQL et
    //    une vue peuvent dépendre l'une de l'autre).
    for overloads in cat.routines.values() {
        for r in overloads {
            if is_system(&r.name.schema) || ext_routines.contains(&r.name) {
                continue;
            }
            if let Some(def) = &r.definition {
                let deps = if r.language == "sql" {
                    references(cat, def)
                } else {
                    // Corps PL/pgSQL : non vérifié à la création.
                    Vec::new()
                };
                graph.push(Node {
                    key: format!("f:{}", r.name),
                    schema: r.name.schema.clone(),
                    order: r.order,
                    deps,
                    sql: def.clone(),
                });
            }
        }
    }
    for r in cat.relations.values() {
        if is_system(&r.name.schema) {
            continue;
        }
        if let (RelationKind::View | RelationKind::MaterializedView, Some(def)) =
            (r.kind, &r.definition)
        {
            graph.push(Node {
                key: format!("r:{}", r.name),
                schema: r.name.schema.clone(),
                order: r.order,
                deps: references(cat, def),
                sql: def.clone(),
            });
        }
    }
    for (rank, node) in sort_by_dependencies(graph).into_iter().enumerate() {
        out.push(6, &node.schema, rank as u64, node.sql);
    }

    // 9. Index.
    for i in cat.indexes.values() {
        if !is_system(&i.name.schema) {
            out.push(9, &i.name.schema, 0, i.definition.clone());
        }
    }
    // 10. Triggers. 11. Policies. 12. Commentaires.
    for t in cat.triggers.values().flat_map(|m| m.values()) {
        if let Some(def) = &t.definition {
            out.push(11, &t.table.schema, 0, def.clone());
        }
    }
    for p in cat.policies.values().flat_map(|m| m.values()) {
        if let Some(def) = &p.definition {
            out.push(12, &p.table.schema, 0, def.clone());
        }
    }
    for c in cat.comments.values() {
        out.push(13, "public", 0, c.clone());
    }

    // Assemblage.
    let mut schemas: BTreeMap<String, String> = BTreeMap::new();
    let mut baseline = String::new();
    let mut exts: Vec<String> = cat
        .extensions
        .values()
        .map(|e| format!("{}@{}", e.name, e.version.clone().unwrap_or_default()))
        .collect();
    exts.sort();
    let _ = write!(baseline, "-- pgweave: postgres={postgres}");
    if !exts.is_empty() {
        let _ = write!(baseline, " extensions={}", exts.join(","));
    }
    baseline
        .push_str("\n-- Migration de référence : état reconstruit par `pgweave schema import`.\n");

    let mut phases: Vec<_> = out.phases.into_iter().collect();
    phases.sort_by(|a, b| a.0.cmp(&b.0));
    for ((_, schema), mut items) in phases {
        items.sort_by_key(|(order, _)| *order);
        let file = schemas.entry(schema).or_default();
        for (_, sql) in items {
            let sql = if sql.trim_end().ends_with(';') {
                sql
            } else {
                format!("{sql};")
            };
            file.push_str(&sql);
            file.push_str("\n\n");
            baseline.push('\n');
            baseline.push_str(&sql);
            baseline.push('\n');
        }
    }
    Dump { schemas, baseline }
}

/// Corps d'une contrainte : sa définition conservée, ou reconstruite.
fn constraint_body(c: &crate::model::Constraint) -> String {
    if let Some(d) = &c.definition {
        return d.clone();
    }
    let cols = |v: &[String]| {
        v.iter()
            .map(|c| quote_ident(c))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match &c.kind {
        ConstraintKind::PrimaryKey { columns } => format!("PRIMARY KEY ({})", cols(columns)),
        ConstraintKind::Unique { columns } => format!("UNIQUE ({})", cols(columns)),
        ConstraintKind::ForeignKey {
            columns,
            references,
            ref_columns,
        } => format!(
            "FOREIGN KEY ({}) REFERENCES {} ({})",
            cols(columns),
            name_sql(references),
            cols(ref_columns)
        ),
        ConstraintKind::Check { expr, .. } => format!("CHECK ({expr})"),
        ConstraintKind::Exclude { definition, .. } => definition.clone(),
    }
}

fn create_table(cat: &Catalog, r: &Relation) -> String {
    let mut lines: Vec<String> = Vec::new();
    for c in &r.columns {
        // `serial` : type entier, défaut tiré de la séquence possédée.
        let owned_seq = cat.sequences.values().find(|s| {
            s.definition.is_none() && s.owned_by.as_ref() == Some(&(r.name.clone(), c.name.clone()))
        });
        let serial = match (&c.ty, owned_seq, &c.default, c.identity) {
            (TypeRef::Named { name, .. }, Some(seq), Some(d), None)
                if name.schema == "pg_catalog"
                    && d.starts_with("nextval(")
                    && d.contains(&seq.name.name) =>
            {
                match name.name.as_str() {
                    "int2" => Some("smallserial"),
                    "int4" => Some("serial"),
                    "int8" => Some("bigserial"),
                    _ => None,
                }
            }
            _ => None,
        };
        let mut line = format!(
            "  {} {}",
            quote_ident(&c.name),
            serial
                .map(str::to_string)
                .unwrap_or_else(|| type_sql(&c.ty))
        );
        if serial.is_none() {
            if let Some(g) = &c.generated {
                let _ = write!(
                    line,
                    " GENERATED ALWAYS AS ({}) {}",
                    g.expr,
                    if g.stored { "STORED" } else { "VIRTUAL" }
                );
            }
            match c.identity {
                Some(IdentityKind::Always) => line.push_str(" GENERATED ALWAYS AS IDENTITY"),
                Some(IdentityKind::ByDefault) => line.push_str(" GENERATED BY DEFAULT AS IDENTITY"),
                None => {}
            }
            if let Some(d) = &c.default {
                let _ = write!(line, " DEFAULT {d}");
            }
            if c.not_null && c.identity.is_none() {
                line.push_str(" NOT NULL");
            }
        }
        lines.push(line);
    }
    for c in &r.constraints {
        if matches!(c.kind, ConstraintKind::ForeignKey { .. }) || c.not_valid {
            continue;
        }
        lines.push(format!(
            "  CONSTRAINT {} {}",
            quote_ident(&c.name),
            constraint_body(c)
        ));
    }
    let kind = match r.persistence {
        crate::model::Persistence::Unlogged => "UNLOGGED ",
        _ => "",
    };
    format!(
        "CREATE {kind}TABLE {} (\n{}\n);",
        name_sql(&r.name),
        lines.join(",\n")
    )
}

/// Objet ordonné par ses dépendances : table, routine ou vue.
struct Node {
    key: String,
    schema: String,
    order: u64,
    deps: Vec<String>,
    sql: String,
}

/// Tri topologique ; l'ordre de création départage, et rompt un éventuel
/// cycle (que PostgreSQL n'aurait de toute façon pas accepté tel quel).
fn sort_by_dependencies(mut nodes: Vec<Node>) -> Vec<Node> {
    nodes.sort_by_key(|n| n.order);
    let keys: BTreeSet<String> = nodes.iter().map(|n| n.key.clone()).collect();
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    let mut pending = nodes;
    while !pending.is_empty() {
        let ready = pending.iter().position(|n| {
            n.deps
                .iter()
                .all(|d| d == &n.key || done.contains(d) || !keys.contains(d))
        });
        let i = ready.unwrap_or(0);
        let n = pending.remove(i);
        done.insert(n.key.clone());
        out.push(n);
    }
    out
}

/// Relations et routines du projet citées par un texte SQL (définition de
/// vue ou de fonction SQL, expression) : noms de `FROM` et appels de
/// fonction, résolus par le `search_path` par défaut.
fn references(cat: &Catalog, sql: &str) -> Vec<String> {
    let Ok(stmts) = pgweave_parser::parse(sql) else {
        return Vec::new();
    };
    let mut names: Vec<(bool, Vec<String>)> = Vec::new();
    for st in &stmts {
        if let Ok(v) = serde_json::to_value(st) {
            collect_names(&v, &mut names);
        }
        // Corps d'une fonction SQL.
        if let pgweave_ast::Statement::Ddl(pgweave_ast::DdlStatement::CreateFunction(f)) = st
            && let Ok(v) = serde_json::to_value(&f.body)
        {
            collect_names(&v, &mut names);
        }
    }
    let path = crate::catalog::SearchPath::default();
    let mut out = Vec::new();
    for (is_fn, parts) in names {
        let (schema, name) = match parts.as_slice() {
            [n] => (None, n.as_str()),
            [s, n] => (Some(s.as_str()), n.as_str()),
            _ => continue,
        };
        if is_fn {
            if let Some(r) = cat.resolve_routines(schema, name, &path).first() {
                out.push(format!("f:{}", r.name));
            }
        } else if let Some(r) = cat.resolve_relation(schema, name, &path) {
            out.push(format!("r:{}", r.name));
        }
    }
    out
}

/// Parcourt l'AST sérialisé : `{"Table": [idents]}` (source de `FROM`) et
/// `{"FunctionCall": {"name": [idents], …}}`.
fn collect_names(v: &serde_json::Value, out: &mut Vec<(bool, Vec<String>)>) {
    let idents = |a: &serde_json::Value| -> Option<Vec<String>> {
        a.as_array()?
            .iter()
            .map(|i| i.get("value").and_then(|v| v.as_str()).map(str::to_string))
            .collect()
    };
    match v {
        serde_json::Value::Object(map) => {
            if let Some(t) = map.get("Table")
                && let Some(n) = idents(t)
            {
                out.push((false, n));
            }
            if let Some(f) = map.get("FunctionCall")
                && let Some(n) = f.get("name").and_then(idents)
            {
                out.push((true, n));
            }
            for child in map.values() {
                collect_names(child, out);
            }
        }
        serde_json::Value::Array(items) => items.iter().for_each(|i| collect_names(i, out)),
        _ => {}
    }
}

/// Dépendances d'une table : routines appelées par ses défauts, colonnes
/// générées et contraintes CHECK.
fn table_deps(cat: &Catalog, r: &Relation) -> Vec<String> {
    let mut exprs: Vec<String> = Vec::new();
    for c in &r.columns {
        exprs.extend(c.default.clone());
        exprs.extend(c.generated.as_ref().map(|g| g.expr.clone()));
    }
    for c in &r.constraints {
        if let ConstraintKind::Check { expr, .. } = &c.kind {
            exprs.push(expr.clone());
        }
    }
    exprs
        .iter()
        .flat_map(|e| references(cat, &format!("SELECT {e}")))
        .filter(|k| k.starts_with("f:"))
        .collect()
}
