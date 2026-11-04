//! Migration diff on the catalog model (PLAN.md, D10 and P7).
//!
//! Two rebuilt catalogs — the state reached by the migrations and the target
//! state described by the declarative schema — are compared object by
//! object; the difference is emitted as statements ordered by their
//! dependencies:
//!
//! 1. what must disappear before any change (policies, triggers, views to
//!    rebuild, modified or removed indexes and constraints);
//! 2. creations and modifications, from schemas to comments, with tables,
//!    routines and views sorted together by dependencies;
//! 3. final drops (columns, tables, routines, types, extensions, schemas),
//!    last: nothing that remains depends on them anymore.
//!
//! Only project objects are compared: neither the built-in catalog nor
//! extension members. A change no statement can express losslessly (removed
//! enum value, base type of a domain…) is never approximated: it is returned
//! in [`Migration::unsupported`].

use std::collections::{BTreeMap, BTreeSet};

use pgzest_ast::emit::emit_statement;
use pgzest_ast::{DdlStatement, Statement, UtilityKind};

use crate::catalog::{Catalog, SearchPath};
use crate::dump::{
    Node, column_sql, constraint_body, create_table, create_type, is_system, name_sql, references,
    sort_by_dependencies, table_deps, type_sql,
};
use crate::model::{
    Column, Constraint, ConstraintKind, QName, Relation, RelationKind, Routine, RoutineKind,
    TypeDef, TypeKind, quote_sql_ident as quote_ident,
};
use crate::pack::signature;

/// One statement of the migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub sql: String,
    pub risks: Vec<Risk>,
}

impl Step {
    fn new(sql: String) -> Self {
        Step {
            sql,
            risks: Vec::new(),
        }
    }

    /// Applying it destroys data: the migration is not applied without
    /// review.
    pub fn is_destructive(&self) -> bool {
        self.risks.iter().any(|r| r.destructive)
    }
}

/// Risk of a statement applied to a populated database (`M0xx` safety
/// rules).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Risk {
    pub code: &'static str,
    pub message: String,
    /// Lost data (table, column).
    pub destructive: bool,
}

impl Risk {
    fn new(code: &'static str, message: String) -> Self {
        Risk {
            code,
            message,
            destructive: false,
        }
    }

    fn destructive(code: &'static str, message: String) -> Self {
        Risk {
            code,
            message,
            destructive: true,
        }
    }

    /// To review before applying: data loss, or a likely failure on a
    /// populated table (NOT NULL column without default, type conversion,
    /// `SET NOT NULL`).
    pub fn needs_review(&self) -> bool {
        self.destructive
            || matches!(
                self.code,
                M001_NOT_NULL_WITHOUT_DEFAULT | M004_COLUMN_TYPE | M005_SET_NOT_NULL
            )
    }
}

/// `NOT NULL` column added without a default: fails if the table has rows.
pub const M001_NOT_NULL_WITHOUT_DEFAULT: &str = "M001";
/// Dropped column, with its data.
pub const M002_DROP_COLUMN: &str = "M002";
/// Dropped table, with its data.
pub const M003_DROP_TABLE: &str = "M003";
/// Column type changed: table rewritten, conversion possibly impossible.
pub const M004_COLUMN_TYPE: &str = "M004";
/// `SET NOT NULL`: full scan, fails on a null value.
pub const M005_SET_NOT_NULL: &str = "M005";
/// Index built without `CONCURRENTLY` on an existing table: writes blocked
/// during the build.
pub const M006_BLOCKING_INDEX: &str = "M006";
/// Constraint added to an existing table, checked on every row under lock
/// (`NOT VALID` then `VALIDATE` avoids it).
pub const M007_VALIDATED_CONSTRAINT: &str = "M007";

/// Change no generated statement expresses: to write by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    pub object: String,
    pub change: String,
}

#[derive(Debug, Clone, Default)]
pub struct Migration {
    pub steps: Vec<Step>,
    pub unsupported: Vec<Unsupported>,
    /// Remarks with no effect on the reached state (column order…).
    pub notes: Vec<String>,
}

impl Migration {
    /// No gap: both catalogs describe the same state.
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty() && self.unsupported.is_empty()
    }

    pub fn to_sql(&self) -> String {
        self.steps
            .iter()
            .map(|s| s.sql.as_str())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

// ── Phases ────────────────────────────────────────────────────────────────────

const DROP_POLICY: u8 = 10;
const DROP_TRIGGER: u8 = 11;
const DROP_VIEW: u8 = 12;
const DROP_REBUILT_ROUTINE: u8 = 13;
const DROP_INDEX: u8 = 14;
const DROP_FOREIGN_KEY: u8 = 15;
const DROP_CONSTRAINT: u8 = 16;
const CREATE_SCHEMA: u8 = 20;
const EXTENSION: u8 = 21;
const CREATE_TYPE: u8 = 22;
const CREATE_DOMAIN: u8 = 23;
const CREATE_SEQUENCE: u8 = 24;
const OBJECTS: u8 = 25;
const CREATE_INDEX: u8 = 27;
const ADD_FOREIGN_KEY: u8 = 28;
const CREATE_TRIGGER: u8 = 29;
const CREATE_POLICY: u8 = 30;
const ROW_SECURITY: u8 = 31;
const COMMENT: u8 = 32;
const DROP_COLUMN: u8 = 40;
const DROP_TABLE: u8 = 41;
const DROP_ROUTINE: u8 = 42;
const DROP_SEQUENCE: u8 = 43;
const DROP_DOMAIN: u8 = 44;
const DROP_TYPE: u8 = 45;
const DROP_EXTENSION: u8 = 46;
const DROP_SCHEMA: u8 = 47;

#[derive(Default)]
struct Out {
    /// (phase, rank within the phase, statement).
    steps: Vec<(u8, u64, Step)>,
    unsupported: Vec<Unsupported>,
    notes: Vec<String>,
    /// Tables, routines and views to create or modify, sorted together.
    graph: Vec<Node>,
    /// Drops of relations and routines, sorted by dependencies in the
    /// starting catalog then reversed.
    drops: Vec<(u8, Node)>,
    /// Risks of a graph statement, by text.
    risks: BTreeMap<String, Vec<Risk>>,
}

impl Out {
    fn push(&mut self, phase: u8, sql: String) {
        self.push_step(phase, Step::new(sql));
    }

    fn push_step(&mut self, phase: u8, step: Step) {
        let rank = self.steps.len() as u64;
        self.steps.push((phase, rank, step));
    }

    fn unsupported(&mut self, object: impl ToString, change: impl Into<String>) {
        self.unsupported.push(Unsupported {
            object: object.to_string(),
            change: change.into(),
        });
    }
}

/// Instructions leading from state `old` to state `new`.
pub fn diff(old: &Catalog, new: &Catalog) -> Migration {
    let o = Objects::of(old);
    let n = Objects::of(new);
    let mut out = Out::default();

    let rebuilt_routines = diff_routines(old, new, &o, &n, &mut out);
    let rebuilt_views = views_to_rebuild(old, &o, &n, &rebuilt_routines);

    diff_schemas(old, new, &mut out);
    diff_extensions(old, new, &mut out);
    diff_types(&o, &n, &mut out);
    diff_sequences(&o, &n, &mut out);
    diff_relations(old, new, &o, &n, &rebuilt_views, &mut out);
    diff_indexes(&o, &n, &rebuilt_views, &mut out);
    diff_triggers(&o, &n, &rebuilt_views, &rebuilt_routines, &mut out);
    diff_policies(&o, &n, &mut out);
    diff_comments(old, new, &rebuilt_views, &mut out);

    // Creations and modifications: a single sort by dependencies.
    for (rank, node) in sort_by_dependencies(std::mem::take(&mut out.graph))
        .into_iter()
        .enumerate()
    {
        let risks = out.risks.remove(&node.sql).unwrap_or_default();
        out.steps.push((
            OBJECTS,
            rank as u64,
            Step {
                sql: node.sql,
                risks,
            },
        ));
    }
    // Drops: creation order, reversed.
    let mut by_phase: BTreeMap<u8, Vec<Node>> = BTreeMap::new();
    for (phase, node) in std::mem::take(&mut out.drops) {
        by_phase.entry(phase).or_default().push(node);
    }
    for (phase, nodes) in by_phase {
        let sorted = sort_by_dependencies(nodes);
        let len = sorted.len() as u64;
        for (rank, node) in sorted.into_iter().enumerate() {
            let risks = match phase {
                DROP_TABLE => vec![Risk::destructive(
                    M003_DROP_TABLE,
                    format!(
                        "table {} dropped, with its rows: rename → ALTER TABLE … RENAME TO; data to keep → copy it first",
                        node.key_name()
                    ),
                )],
                _ => Vec::new(),
            };
            out.steps.push((
                phase,
                len - rank as u64,
                Step {
                    sql: node.sql,
                    risks,
                },
            ));
        }
    }

    out.steps.sort_by_key(|(phase, rank, _)| (*phase, *rank));
    Migration {
        steps: out
            .steps
            .into_iter()
            .map(|(_, _, mut s)| {
                // Kept definitions do not all have their `;`.
                if !s.sql.trim_end().ends_with(';') {
                    s.sql = format!("{};", s.sql.trim_end());
                }
                s
            })
            .collect(),
        unsupported: out.unsupported,
        notes: out.notes,
    }
}

/// Migration between two SQL scripts, each replayed on the base catalog
/// (without view analysis: their columns are not typed).
pub fn diff_sql(old: &str, new: &str) -> Result<Migration, String> {
    let before = crate::ProjectCatalog::from_sql(old)?;
    let after = crate::ProjectCatalog::from_sql(new)?;
    Ok(diff(&before.catalog, &after.catalog))
}

impl Node {
    fn key_name(&self) -> &str {
        self.key.split_once(':').map_or(&self.key, |(_, n)| n)
    }
}

// ── Project objects ───────────────────────────────────────────────────────────

/// Project objects of a catalog, indexed by identity.
struct Objects<'a> {
    types: BTreeMap<&'a QName, &'a TypeDef>,
    routines: BTreeMap<(QName, Vec<String>), &'a Routine>,
    /// Tables and views.
    relations: BTreeMap<&'a QName, &'a Relation>,
    indexes: BTreeMap<&'a QName, &'a crate::model::Index>,
    /// Explicitly created sequences.
    sequences: BTreeMap<&'a QName, &'a crate::model::Sequence>,
    triggers: BTreeMap<(&'a QName, &'a str), &'a crate::model::Trigger>,
    policies: BTreeMap<(&'a QName, &'a str), &'a crate::model::Policy>,
}

impl<'a> Objects<'a> {
    fn of(cat: &'a Catalog) -> Self {
        let packs = cat.extension_packs.values();
        let ext_types: BTreeSet<&QName> = packs
            .clone()
            .flat_map(|p| p.types.iter().map(|t| &t.name))
            .collect();
        let ext_routines: BTreeSet<(QName, Vec<String>)> = packs
            .clone()
            .flat_map(|p| p.routines.iter().map(|r| (r.name.clone(), signature(r))))
            .collect();
        let ext_relations: BTreeSet<&QName> = packs
            .flat_map(|p| p.relations.iter().map(|r| &r.name))
            .collect();
        let mine = |q: &QName| !is_system(&q.schema);
        Objects {
            types: cat
                .types
                .iter()
                .filter(|(q, t)| {
                    mine(q)
                        && !ext_types.contains(q)
                        // Row type of a relation: follows the relation.
                        && !cat.relations.contains_key(q)
                        && matches!(
                            t.kind,
                            TypeKind::Enum { .. } | TypeKind::Composite { .. } | TypeKind::Domain { .. }
                        )
                })
                .collect(),
            routines: cat
                .routines
                .values()
                .flatten()
                .filter(|r| mine(&r.name))
                .map(|r| ((r.name.clone(), signature(r)), r))
                .filter(|(k, _)| !ext_routines.contains(k))
                .collect(),
            relations: cat
                .relations
                .iter()
                .filter(|(q, _)| mine(q) && !ext_relations.contains(q))
                .collect(),
            indexes: cat
                .indexes
                .iter()
                .filter(|(q, i)| mine(q) && !ext_relations.contains(&i.table))
                .collect(),
            sequences: cat
                .sequences
                .iter()
                .filter(|(q, s)| mine(q) && s.definition.is_some())
                .collect(),
            triggers: cat
                .triggers
                .iter()
                .flat_map(|(t, m)| m.iter().map(move |(n, tr)| ((t, n.as_str()), tr)))
                .filter(|((t, _), _)| mine(t))
                .collect(),
            policies: cat
                .policies
                .iter()
                .flat_map(|(t, m)| m.iter().map(move |(n, p)| ((t, n.as_str()), p)))
                .filter(|((t, _), _)| mine(t))
                .collect(),
        }
    }
}

fn quote_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// ── Schemas and extensions ────────────────────────────────────────────────────

fn diff_schemas(old: &Catalog, new: &Catalog, out: &mut Out) {
    let user = |c: &Catalog| -> BTreeSet<String> {
        c.schemas
            .keys()
            .filter(|s| !is_system(s) && *s != "public")
            .cloned()
            .collect()
    };
    let (before, after) = (user(old), user(new));
    for s in after.difference(&before) {
        let owner = new.schemas[s]
            .owner
            .as_ref()
            .map(|o| format!(" AUTHORIZATION {}", quote_ident(o)))
            .unwrap_or_default();
        out.push(
            CREATE_SCHEMA,
            format!("CREATE SCHEMA {}{owner};", quote_ident(s)),
        );
    }
    for s in before.difference(&after) {
        out.push(DROP_SCHEMA, format!("DROP SCHEMA {};", quote_ident(s)));
    }
    for s in before.intersection(&after) {
        if old.schemas[s].owner != new.schemas[s].owner {
            match &new.schemas[s].owner {
                Some(o) => out.push(
                    CREATE_SCHEMA,
                    format!(
                        "ALTER SCHEMA {} OWNER TO {};",
                        quote_ident(s),
                        quote_ident(o)
                    ),
                ),
                None => out.unsupported(
                    format!("schema {s}"),
                    "owner removed from the declarative schema: a schema's owner cannot be removed",
                ),
            }
        }
    }
}

fn diff_extensions(old: &Catalog, new: &Catalog, out: &mut Out) {
    for (name, e) in &new.extensions {
        match old.extensions.get(name) {
            None => {
                let version = e
                    .version
                    .as_ref()
                    .map(|v| format!(" VERSION {}", quote_literal(v)))
                    .unwrap_or_default();
                out.push(
                    EXTENSION,
                    format!(
                        "CREATE EXTENSION IF NOT EXISTS {} SCHEMA {}{version};",
                        quote_ident(name),
                        quote_ident(&e.schema)
                    ),
                );
            }
            Some(before) => {
                if before.schema != e.schema {
                    out.push(
                        EXTENSION,
                        format!(
                            "ALTER EXTENSION {} SET SCHEMA {};",
                            quote_ident(name),
                            quote_ident(&e.schema)
                        ),
                    );
                }
                if before.version != e.version {
                    match &e.version {
                        Some(v) => out.push(
                            EXTENSION,
                            format!(
                                "ALTER EXTENSION {} UPDATE TO {};",
                                quote_ident(name),
                                quote_literal(v)
                            ),
                        ),
                        None => out.unsupported(
                            format!("extension {name}"),
                            "version removed from the declarative schema: the target version is unknown",
                        ),
                    }
                }
            }
        }
    }
    for name in old.extensions.keys() {
        if !new.extensions.contains_key(name) {
            out.push(
                DROP_EXTENSION,
                format!("DROP EXTENSION {};", quote_ident(name)),
            );
        }
    }
}

// ── Types ─────────────────────────────────────────────────────────────────────

fn create_type_sql(t: &TypeDef) -> Option<(u8, String)> {
    let phase = match t.kind {
        TypeKind::Domain { .. } => CREATE_DOMAIN,
        _ => CREATE_TYPE,
    };
    Some((phase, create_type(t)?))
}

fn diff_types(o: &Objects, n: &Objects, out: &mut Out) {
    for (q, t) in &n.types {
        match o.types.get(q) {
            None => {
                if let Some((phase, sql)) = create_type_sql(t) {
                    out.push(phase, sql);
                }
            }
            Some(before) if before.kind != t.kind => alter_type(before, t, out),
            Some(_) => {}
        }
    }
    for (q, t) in &o.types {
        if !n.types.contains_key(q) {
            let (phase, kind) = match t.kind {
                TypeKind::Domain { .. } => (DROP_DOMAIN, "DOMAIN"),
                _ => (DROP_TYPE, "TYPE"),
            };
            out.push(phase, format!("DROP {kind} {};", name_sql(q)));
        }
    }
}

fn alter_type(before: &TypeDef, after: &TypeDef, out: &mut Out) {
    let q = &after.name;
    match (&before.kind, &after.kind) {
        (TypeKind::Enum { labels: old }, TypeKind::Enum { labels: new }) => {
            // Existing values must remain, in the same order: PostgreSQL can
            // neither remove nor move a value.
            let kept: Vec<&String> = new.iter().filter(|l| old.contains(l)).collect();
            if kept.len() != old.len() || kept.iter().zip(old).any(|(a, b)| *a != b) {
                out.unsupported(
                    format!("type {q}"),
                    "enum values removed or reordered: PostgreSQL cannot remove an enum value (recreate the type and convert the columns)",
                );
                return;
            }
            for (i, label) in new.iter().enumerate() {
                if old.contains(label) {
                    continue;
                }
                let position = match i {
                    0 => new
                        .get(1)
                        .map(|next| format!(" BEFORE {}", quote_literal(next))),
                    _ => Some(format!(" AFTER {}", quote_literal(&new[i - 1]))),
                };
                out.push(
                    CREATE_TYPE,
                    format!(
                        "ALTER TYPE {} ADD VALUE {}{};",
                        name_sql(q),
                        quote_literal(label),
                        position.unwrap_or_default()
                    ),
                );
            }
            out.notes.push(format!(
                "values added to enum {q}: PostgreSQL refuses to use them in the transaction that adds them"
            ));
        }
        (TypeKind::Domain { .. }, TypeKind::Domain { .. }) => out.unsupported(
            format!("domain {q}"),
            "definition changed (base type, default, NOT NULL or CHECK): ALTER DOMAIN is not generated yet",
        ),
        (TypeKind::Composite { .. }, TypeKind::Composite { .. }) => out.unsupported(
            format!("type {q}"),
            "attributes changed: ALTER TYPE … ATTRIBUTE is not generated yet",
        ),
        _ => out.unsupported(
            format!("type {q}"),
            "type kind changed (enum, composite, domain): recreate it by hand",
        ),
    }
}

// ── Sequences ─────────────────────────────────────────────────────────────────

fn diff_sequences(o: &Objects, n: &Objects, out: &mut Out) {
    for (q, s) in &n.sequences {
        match o.sequences.get(q) {
            None => out.push(CREATE_SEQUENCE, s.definition.clone().unwrap_or_default()),
            Some(before) if before.definition != s.definition => out.unsupported(
                format!("sequence {q}"),
                "parameters changed: ALTER SEQUENCE is not generated yet",
            ),
            Some(_) => {}
        }
    }
    for q in o.sequences.keys() {
        if !n.sequences.contains_key(q) {
            out.push(DROP_SEQUENCE, format!("DROP SEQUENCE {};", name_sql(q)));
        }
    }
}

// ── Routines ──────────────────────────────────────────────────────────────────

fn routine_kind_sql(r: &Routine) -> &'static str {
    match r.kind {
        RoutineKind::Procedure => "PROCEDURE",
        RoutineKind::Aggregate => "AGGREGATE",
        _ => "FUNCTION",
    }
}

fn drop_routine_sql(r: &Routine) -> String {
    let args: Vec<String> = r.input_types().map(type_sql).collect();
    format!(
        "DROP {} {}({});",
        routine_kind_sql(r),
        name_sql(&r.name),
        args.join(", ")
    )
}

/// Comparable definition: `OR REPLACE` is not a property of the object (a
/// replaced routine keeps the text of its last `CREATE`).
fn same_definition(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a == b || normalized(a) == normalized(b),
        (a, b) => a == b,
    }
}

fn normalized(definition: &str) -> Option<String> {
    let mut stmt = pgzest_parser::parse(definition).ok()?.pop()?;
    match &mut stmt {
        Statement::Ddl(DdlStatement::CreateFunction(f)) => f.or_replace = false,
        Statement::Ddl(DdlStatement::CreateProcedure(p)) => p.or_replace = false,
        Statement::Ddl(DdlStatement::CreateView(v)) => v.or_replace = false,
        Statement::Ddl(DdlStatement::CreateTrigger(t)) => t.or_replace = false,
        _ => {}
    }
    Some(emit_statement(&stmt))
}

/// Same routine: same written text when both are known (the body is kept
/// verbatim, an edited comment counts), same definition otherwise.
fn same_routine(a: &Routine, b: &Routine) -> bool {
    fn body(src: &str) -> &str {
        let rest = src.trim_start();
        let rest = rest
            .get(6..)
            .filter(|_| rest[..6].eq_ignore_ascii_case("CREATE"))
            .unwrap_or(rest);
        let rest = rest.trim_start();
        match rest.get(..2) {
            Some(w) if w.eq_ignore_ascii_case("OR") => {
                let r = rest[2..].trim_start();
                r.get(7..)
                    .filter(|_| r[..7].eq_ignore_ascii_case("REPLACE"))
                    .unwrap_or(r)
            }
            _ => rest,
        }
    }
    match (&a.source, &b.source) {
        (Some(x), Some(y)) => body(x).trim() == body(y).trim(),
        _ => same_definition(a.definition.as_deref(), b.definition.as_deref()),
    }
}

/// Text to emit for a routine: the one it was written with, otherwise its
/// emitted definition.
fn routine_sql(r: &Routine) -> Option<String> {
    r.source.clone().or_else(|| r.definition.clone())
}

/// `CREATE OR REPLACE` of a routine, on its written text when known
/// (`CREATE` → `CREATE OR REPLACE`, checked by reading it back).
fn routine_or_replace(r: &Routine) -> Option<String> {
    let def = r.definition.as_deref()?;
    if let Some(src) = &r.source {
        let rest = src.trim_start();
        if rest.len() >= 6 && rest[..6].eq_ignore_ascii_case("CREATE") {
            let after = &rest[6..];
            let already = after
                .trim_start()
                .get(..2)
                .is_some_and(|w| w.eq_ignore_ascii_case("OR"));
            let text = if already {
                rest.to_string()
            } else {
                format!("CREATE OR REPLACE{after}")
            };
            let expected = or_replace(def)?;
            let same = pgzest_parser::parse(&text)
                .is_ok_and(|v| v.len() == 1 && emit_statement(&v[0]) == expected);
            if same {
                return Some(text);
            }
        }
    }
    or_replace(def)
}

/// `CREATE OR REPLACE` of an emitted definition.
fn or_replace(definition: &str) -> Option<String> {
    let mut stmts = pgzest_parser::parse(definition).ok()?;
    let mut stmt = stmts.pop()?;
    match &mut stmt {
        Statement::Ddl(DdlStatement::CreateFunction(f)) => f.or_replace = true,
        Statement::Ddl(DdlStatement::CreateProcedure(p)) => p.or_replace = true,
        Statement::Ddl(DdlStatement::CreateView(v)) if !v.materialized => v.or_replace = true,
        _ => return None,
    }
    Some(emit_statement(&stmt))
}

/// `CREATE OR REPLACE` keeps the output signature and the input parameter
/// names: otherwise, PostgreSQL requires dropping first.
fn replaceable(before: &Routine, after: &Routine) -> bool {
    let inputs = |r: &Routine| -> Vec<Option<String>> {
        r.args
            .iter()
            .filter(|a| {
                !matches!(
                    a.mode,
                    crate::model::ArgMode::Out | crate::model::ArgMode::Table
                )
            })
            .map(|a| a.name.clone())
            .collect()
    };
    let outputs = |r: &Routine| -> Vec<(Option<String>, crate::model::TypeRef)> {
        r.args
            .iter()
            .filter(|a| {
                !matches!(
                    a.mode,
                    crate::model::ArgMode::In | crate::model::ArgMode::Variadic
                )
            })
            .map(|a| (a.name.clone(), a.ty.clone()))
            .collect()
    };
    before.kind == after.kind
        && before.returns == after.returns
        && before.returns_set == after.returns_set
        && inputs(before) == inputs(after)
        && outputs(before) == outputs(after)
        // A removed argument default: rejected by CREATE OR REPLACE.
        && before
            .args
            .iter()
            .zip(&after.args)
            .all(|(a, b)| !a.has_default || b.has_default)
}

fn routine_node(cat: &Catalog, r: &Routine, sql: String) -> Node {
    let deps = match &r.definition {
        Some(def) if r.language == "sql" => references(cat, def),
        _ => Vec::new(),
    };
    Node {
        key: format!("f:{}", r.name),
        schema: r.name.schema.clone(),
        order: r.order,
        deps,
        sql,
    }
}

/// Returns the routines dropped then recreated (output signature changed).
fn diff_routines(
    old: &Catalog,
    new: &Catalog,
    o: &Objects,
    n: &Objects,
    out: &mut Out,
) -> BTreeSet<QName> {
    let mut rebuilt = BTreeSet::new();
    for (key, r) in &n.routines {
        let Some(def) = routine_sql(r) else {
            if !o.routines.contains_key(key) {
                out.unsupported(
                    format!("routine {}", r.name),
                    "created without a replayable definition (CREATE AGGREGATE…): to write by hand",
                );
            }
            continue;
        };
        match o.routines.get(key) {
            None => out.graph.push(routine_node(new, r, def)),
            Some(before) if same_routine(before, r) => {}
            Some(before) => match routine_or_replace(r) {
                Some(sql) if replaceable(before, r) => out.graph.push(routine_node(new, r, sql)),
                _ => {
                    rebuilt.insert(r.name.clone());
                    out.push(DROP_REBUILT_ROUTINE, drop_routine_sql(before));
                    out.graph.push(routine_node(new, r, def));
                }
            },
        }
    }
    for (key, r) in &o.routines {
        if !n.routines.contains_key(key) {
            if r.definition.is_none() {
                out.unsupported(
                    format!("routine {}", r.name),
                    "dropped, with no known definition: to drop by hand",
                );
                continue;
            }
            out.drops.push((
                DROP_ROUTINE,
                Node {
                    key: format!("f:{}", r.name),
                    schema: r.name.schema.clone(),
                    order: r.order,
                    deps: routine_node(old, r, String::new()).deps,
                    sql: drop_routine_sql(r),
                },
            ));
        }
    }
    rebuilt
}

// ── Relations ─────────────────────────────────────────────────────────────────

fn is_view(r: &Relation) -> bool {
    matches!(r.kind, RelationKind::View | RelationKind::MaterializedView)
}

/// Columns whose removal or type change blocks the views referencing the
/// table.
fn columns_reshaped(before: &Relation, after: &Relation) -> bool {
    before
        .columns
        .iter()
        .any(|c| after.column(&c.name).is_none_or(|a| a.ty != c.ty))
}

/// Views of the starting state to drop before modifications: removed, not
/// replaceable, or depending on a rebuilt object (view, routine, or table
/// whose column changes type or disappears). Transitive closure: a view
/// referencing a rebuilt view is rebuilt too.
fn views_to_rebuild(
    old: &Catalog,
    o: &Objects,
    n: &Objects,
    rebuilt_routines: &BTreeSet<QName>,
) -> BTreeSet<QName> {
    let mut blocked: BTreeSet<String> = rebuilt_routines.iter().map(|q| format!("f:{q}")).collect();
    for (key, r) in &o.routines {
        if !n.routines.contains_key(key) {
            blocked.insert(format!("f:{}", r.name));
        }
    }
    let mut out: BTreeSet<QName> = BTreeSet::new();
    for (q, before) in &o.relations {
        match n.relations.get(q) {
            None => {
                blocked.insert(format!("r:{q}"));
                if is_view(before) {
                    out.insert((*q).clone());
                }
            }
            Some(after) if is_view(before) => {
                if !same_definition(before.definition.as_deref(), after.definition.as_deref())
                    && !view_replaceable(before, after)
                {
                    out.insert((*q).clone());
                }
            }
            Some(after) => {
                if columns_reshaped(before, after) {
                    blocked.insert(format!("r:{q}"));
                }
            }
        }
    }
    let views: Vec<(&QName, Vec<String>)> = o
        .relations
        .iter()
        .filter(|(_, r)| is_view(r))
        .map(|(q, r)| (*q, references(old, r.definition.as_deref().unwrap_or(""))))
        .collect();
    loop {
        let mut changed = false;
        for (q, deps) in &views {
            let key = format!("r:{q}");
            if out.contains(*q) || deps.iter().any(|d| d != &key && blocked.contains(d)) {
                changed |= out.insert((*q).clone());
                changed |= blocked.insert(key);
            }
        }
        if !changed {
            break;
        }
    }
    out
}

/// `CREATE OR REPLACE VIEW` only accepts columns added at the end, existing
/// ones keeping their name and type.
fn view_replaceable(before: &Relation, after: &Relation) -> bool {
    before.kind == RelationKind::View
        && after.kind == RelationKind::View
        && !before.opaque
        && !after.opaque
        && before.columns.len() <= after.columns.len()
        && before.columns.iter().zip(&after.columns).all(|(a, b)| {
            a.name == b.name && a.ty == b.ty && !matches!(a.ty, crate::model::TypeRef::Unknown)
        })
}

fn view_node(cat: &Catalog, r: &Relation, sql: String) -> Node {
    Node {
        key: format!("r:{}", r.name),
        schema: r.name.schema.clone(),
        order: r.order,
        deps: references(cat, r.definition.as_deref().unwrap_or("")),
        sql,
    }
}

fn diff_relations(
    old: &Catalog,
    new: &Catalog,
    o: &Objects,
    n: &Objects,
    rebuilt_views: &BTreeSet<QName>,
    out: &mut Out,
) {
    // Dropped views or views to rebuild: before any modification.
    for q in rebuilt_views {
        let r = o.relations[q];
        let kind = match r.kind {
            RelationKind::MaterializedView => "MATERIALIZED VIEW",
            _ => "VIEW",
        };
        out.drops.push((
            DROP_VIEW,
            Node {
                key: format!("r:{q}"),
                schema: q.schema.clone(),
                order: r.order,
                deps: references(old, r.definition.as_deref().unwrap_or("")),
                sql: format!("DROP {kind} {};", name_sql(q)),
            },
        ));
    }

    for (q, after) in &n.relations {
        let before = o.relations.get(q).copied();
        if let Some(b) = before
            && is_view(b) != is_view(after)
        {
            out.unsupported(
                format!("relation {q}"),
                "table turned into a view, or the reverse: to migrate by hand",
            );
            continue;
        }
        if is_view(after) {
            let Some(def) = &after.definition else {
                continue;
            };
            match before {
                None => out.graph.push(view_node(new, after, def.clone())),
                Some(_) if rebuilt_views.contains(*q) => {
                    out.graph.push(view_node(new, after, def.clone()))
                }
                Some(b)
                    if !same_definition(b.definition.as_deref(), after.definition.as_deref()) =>
                {
                    let sql = or_replace(def).unwrap_or_else(|| def.clone());
                    out.graph.push(view_node(new, after, sql));
                }
                Some(_) => {}
            }
            continue;
        }
        match before {
            None => {
                out.graph.push(Node {
                    key: format!("r:{q}"),
                    schema: q.schema.clone(),
                    order: after.order,
                    deps: table_deps(new, after),
                    sql: create_table(new, after),
                });
                add_deferred_constraints(after, None, out);
                if after.row_level_security {
                    out.push(
                        ROW_SECURITY,
                        format!("ALTER TABLE {} ENABLE ROW LEVEL SECURITY;", name_sql(q)),
                    );
                }
            }
            Some(b) => alter_table(new, b, after, out),
        }
    }

    for (q, before) in &o.relations {
        if n.relations.contains_key(q) || is_view(before) {
            continue;
        }
        out.drops.push((
            DROP_TABLE,
            Node {
                key: format!("r:{q}"),
                schema: q.schema.clone(),
                order: before.order,
                deps: Vec::new(),
                sql: format!("DROP TABLE {};", name_sql(q)),
            },
        ));
    }
}

fn is_deferred(c: &Constraint) -> bool {
    matches!(c.kind, ConstraintKind::ForeignKey { .. }) || c.not_valid
}

/// Foreign keys and `NOT VALID` constraints of a table: after the indexes (a
/// foreign key may target a unique index).
fn add_deferred_constraints(r: &Relation, before: Option<&Relation>, out: &mut Out) {
    for c in &r.constraints {
        let unchanged = before
            .and_then(|b| b.constraints.iter().find(|x| x.name == c.name))
            .is_some_and(|x| same_constraint(x, c));
        if unchanged || !is_deferred(c) {
            continue;
        }
        let phase = if matches!(c.kind, ConstraintKind::ForeignKey { .. }) {
            ADD_FOREIGN_KEY
        } else {
            CREATE_INDEX
        };
        let mut step = Step::new(add_constraint_sql(&r.name, c));
        if before.is_some() && !c.not_valid {
            step.risks.push(Risk::new(
                M007_VALIDATED_CONSTRAINT,
                format!(
                    "constraint {} checked on every row of {}, under lock",
                    c.name, r.name
                ),
            ));
        }
        out.push_step(phase, step);
    }
}

fn add_constraint_sql(table: &QName, c: &Constraint) -> String {
    format!(
        "ALTER TABLE {} ADD CONSTRAINT {} {}{};",
        name_sql(table),
        quote_ident(&c.name),
        constraint_body(c),
        if c.not_valid { " NOT VALID" } else { "" }
    )
}

fn same_constraint(a: &Constraint, b: &Constraint) -> bool {
    constraint_body(a) == constraint_body(b) && a.not_valid == b.not_valid
}

fn alter_table(new: &Catalog, before: &Relation, after: &Relation, out: &mut Out) {
    let q = &after.name;
    let table = name_sql(q);
    if before.persistence != after.persistence {
        out.unsupported(
            format!("table {q}"),
            "persistence changed (UNLOGGED): ALTER TABLE … SET LOGGED is not generated yet",
        );
    }
    if before.kind != after.kind {
        out.unsupported(
            format!("table {q}"),
            "partitioned or not: to migrate by hand",
        );
    }

    // Removed or modified constraints: before the columns.
    for c in &before.constraints {
        let kept = after.constraints.iter().find(|x| x.name == c.name);
        match kept {
            Some(x) if same_constraint(c, x) => {}
            Some(x) if constraint_body(c) == constraint_body(x) && c.not_valid => {
                // Validated since: only the validation is missing.
                out.graph.push(Node {
                    key: format!("r:{q}"),
                    schema: q.schema.clone(),
                    order: after.order,
                    deps: Vec::new(),
                    sql: format!(
                        "ALTER TABLE {table} VALIDATE CONSTRAINT {};",
                        quote_ident(&c.name)
                    ),
                });
            }
            Some(x) if constraint_body(c) == constraint_body(x) => out.unsupported(
                format!("constraint {} of {q}", c.name),
                "a validated constraint cannot become NOT VALID again",
            ),
            _ => {
                let phase = if matches!(c.kind, ConstraintKind::ForeignKey { .. }) {
                    DROP_FOREIGN_KEY
                } else {
                    DROP_CONSTRAINT
                };
                out.push(
                    phase,
                    format!(
                        "ALTER TABLE {table} DROP CONSTRAINT {};",
                        quote_ident(&c.name)
                    ),
                );
            }
        }
    }

    let mut actions: Vec<String> = Vec::new();
    let mut warnings: Vec<Risk> = Vec::new();
    for c in &after.columns {
        match before.column(&c.name) {
            None => {
                if c.not_null
                    && c.default.is_none()
                    && c.identity.is_none()
                    && c.generated.is_none()
                {
                    warnings.push(Risk::new(
                        M001_NOT_NULL_WITHOUT_DEFAULT,
                        format!(
                            "column {}.{} NOT NULL without default: fails if the table has rows → add a DEFAULT, or add it nullable, fill it, then SET NOT NULL",
                            q, c.name
                        ),
                    ));
                }
                actions.push(format!("ADD COLUMN {}", column_sql(new, after, c)));
            }
            Some(b) => alter_column(q, b, c, &mut actions, &mut warnings, out),
        }
    }
    // Added or modified constraints (except foreign keys and NOT VALID).
    for c in &after.constraints {
        let unchanged = before
            .constraints
            .iter()
            .find(|x| x.name == c.name)
            .is_some_and(|x| same_constraint(x, c) || constraint_body(x) == constraint_body(c));
        if !unchanged && !is_deferred(c) {
            warnings.push(Risk::new(
                M007_VALIDATED_CONSTRAINT,
                format!(
                    "constraint {} checked on every row of {q}, under lock",
                    c.name
                ),
            ));
            actions.push(format!(
                "ADD CONSTRAINT {} {}",
                quote_ident(&c.name),
                constraint_body(c)
            ));
        }
    }
    add_deferred_constraints(after, Some(before), out);
    if !actions.is_empty() {
        let sql = format!("ALTER TABLE {table}\n  {};", actions.join(",\n  "));
        if !warnings.is_empty() {
            out.risks.insert(sql.clone(), warnings);
        }
        out.graph.push(Node {
            key: format!("r:{q}"),
            schema: q.schema.clone(),
            order: after.order,
            deps: table_deps(new, after),
            sql,
        });
    }

    // Removed columns: last, nothing references them anymore.
    let order_kept = |r: &Relation| -> Vec<String> {
        r.columns
            .iter()
            .filter(|c| before.column(&c.name).is_some() && after.column(&c.name).is_some())
            .map(|c| c.name.clone())
            .collect()
    };
    for c in &before.columns {
        if after.column(&c.name).is_none() {
            out.push_step(
                DROP_COLUMN,
                Step {
                    sql: format!("ALTER TABLE {table} DROP COLUMN {};", quote_ident(&c.name)),
                    risks: vec![Risk::destructive(
                        M002_DROP_COLUMN,
                        format!(
                            "column {q}.{} dropped, with its values: rename → RENAME COLUMN; values to keep → copy them first",
                            c.name
                        ),
                    )],
                },
            );
        }
    }
    let appended = after
        .columns
        .iter()
        .skip_while(|c| before.column(&c.name).is_some())
        .any(|c| before.column(&c.name).is_some());
    if order_kept(before) != order_kept(after) || appended {
        out.notes.push(format!(
            "column order of {q}: PostgreSQL adds a column at the end, the declared order is not reproduced"
        ));
    }

    if before.row_level_security != after.row_level_security {
        out.push(
            ROW_SECURITY,
            format!(
                "ALTER TABLE {table} {} ROW LEVEL SECURITY;",
                if after.row_level_security {
                    "ENABLE"
                } else {
                    "DISABLE"
                }
            ),
        );
    }
}

fn alter_column(
    table: &QName,
    before: &Column,
    after: &Column,
    actions: &mut Vec<String>,
    warnings: &mut Vec<Risk>,
    out: &mut Out,
) {
    let col = quote_ident(&after.name);
    if before.identity != after.identity {
        out.unsupported(
            format!("column {table}.{}", after.name),
            "identity added, removed or changed: ALTER COLUMN … IDENTITY is not generated yet",
        );
        return;
    }
    if before.generated != after.generated {
        out.unsupported(
            format!("column {table}.{}", after.name),
            "generated expression changed: ALTER COLUMN … SET EXPRESSION is not generated yet",
        );
        return;
    }
    if before.ty != after.ty {
        warnings.push(Risk::new(
            M004_COLUMN_TYPE,
            format!(
                "column {table}.{}: {} → {} rewrites the table; without an assignment cast, PostgreSQL requires USING",
                after.name,
                type_sql(&before.ty),
                type_sql(&after.ty)
            ),
        ));
        actions.push(format!(
            "ALTER COLUMN {col} SET DATA TYPE {}",
            type_sql(&after.ty)
        ));
    }
    if before.default != after.default {
        match &after.default {
            Some(d) => actions.push(format!("ALTER COLUMN {col} SET DEFAULT {d}")),
            None => actions.push(format!("ALTER COLUMN {col} DROP DEFAULT")),
        }
    }
    if before.not_null != after.not_null && after.identity.is_none() {
        if after.not_null {
            warnings.push(Risk::new(
                M005_SET_NOT_NULL,
                format!(
                    "column {table}.{} NOT NULL: full table scan, fails on a null value → fill the nulls first",
                    after.name
                ),
            ));
            actions.push(format!("ALTER COLUMN {col} SET NOT NULL"));
        } else {
            actions.push(format!("ALTER COLUMN {col} DROP NOT NULL"));
        }
    }
}

// ── Index, triggers, policies ─────────────────────────────────────────────────

fn diff_indexes(o: &Objects, n: &Objects, rebuilt_views: &BTreeSet<QName>, out: &mut Out) {
    let table_dropped = |t: &QName| !n.relations.contains_key(t);
    let create = |i: &crate::model::Index, out: &mut Out| {
        let mut step = Step::new(i.definition.clone());
        let existing = o.relations.get(&i.table).is_some_and(|r| !is_view(r));
        if existing && !i.definition.to_uppercase().contains("CONCURRENTLY") {
            step.risks.push(Risk::new(
                M006_BLOCKING_INDEX,
                format!(
                    "index {} built without CONCURRENTLY: writes on {} blocked during the build",
                    i.name, i.table
                ),
            ));
        }
        out.push_step(CREATE_INDEX, step);
    };
    for (q, i) in &n.indexes {
        match o.indexes.get(q) {
            Some(b) if b.definition == i.definition && !rebuilt_views.contains(&i.table) => {}
            Some(b) => {
                if !rebuilt_views.contains(&b.table) {
                    out.push(DROP_INDEX, format!("DROP INDEX {};", name_sql(q)));
                }
                create(i, out);
            }
            None => create(i, out),
        }
    }
    for (q, i) in &o.indexes {
        if !n.indexes.contains_key(q)
            && !table_dropped(&i.table)
            && !rebuilt_views.contains(&i.table)
        {
            out.push(DROP_INDEX, format!("DROP INDEX {};", name_sql(q)));
        }
    }
}

fn diff_triggers(
    o: &Objects,
    n: &Objects,
    rebuilt_views: &BTreeSet<QName>,
    rebuilt_routines: &BTreeSet<QName>,
    out: &mut Out,
) {
    let drop_sql = |t: &crate::model::Trigger| {
        format!(
            "DROP TRIGGER {} ON {};",
            quote_ident(&t.name),
            name_sql(&t.table)
        )
    };
    for (key, t) in &n.triggers {
        let Some(def) = &t.definition else { continue };
        match o.triggers.get(key) {
            Some(b)
                if same_definition(b.definition.as_deref(), t.definition.as_deref())
                    && !rebuilt_views.contains(&t.table)
                    && !rebuilt_routines.contains(&t.function) => {}
            Some(b) => {
                if !rebuilt_views.contains(&b.table) && !rebuilt_routines.contains(&b.function) {
                    out.push(DROP_TRIGGER, drop_sql(b));
                }
                out.push(CREATE_TRIGGER, def.clone());
            }
            None => out.push(CREATE_TRIGGER, def.clone()),
        }
    }
    for (key, t) in &o.triggers {
        if !n.triggers.contains_key(key)
            && n.relations.contains_key(&t.table)
            && !rebuilt_views.contains(&t.table)
        {
            out.push(DROP_TRIGGER, drop_sql(t));
        }
    }
}

fn diff_policies(o: &Objects, n: &Objects, out: &mut Out) {
    let drop_sql = |p: &crate::model::Policy| {
        format!(
            "DROP POLICY {} ON {};",
            quote_ident(&p.name),
            name_sql(&p.table)
        )
    };
    for (key, p) in &n.policies {
        let Some(def) = &p.definition else { continue };
        match o.policies.get(key) {
            Some(b) if b.definition == p.definition => {}
            Some(b) => {
                out.push(DROP_POLICY, drop_sql(b));
                out.push(CREATE_POLICY, def.clone());
            }
            None => out.push(CREATE_POLICY, def.clone()),
        }
    }
    for (key, p) in &o.policies {
        if !n.policies.contains_key(key) && n.relations.contains_key(&p.table) {
            out.push(DROP_POLICY, drop_sql(p));
        }
    }
}

// ── Comments ──────────────────────────────────────────────────────────────────

/// Target of an emitted `COMMENT ON`: (kind, written name).
fn comment_target(sql: &str) -> Option<(pgzest_ast::utility::CommentTarget, Vec<String>)> {
    let stmt = pgzest_parser::parse(sql).ok()?.pop()?;
    match stmt {
        Statement::Utility(u) => match u.kind {
            UtilityKind::Comment { target, name, .. } => {
                Some((target, name.0.iter().map(|i| i.value.clone()).collect()))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Does the commented object exist in `cat`? A dropped object takes its
/// comment with it: there is nothing to remove.
fn comment_object_exists(cat: &Catalog, sql: &str) -> bool {
    use pgzest_ast::utility::CommentTarget as T;
    let Some((target, parts)) = comment_target(sql) else {
        return false;
    };
    let path = SearchPath::default();
    let split = |p: &[String]| -> (Option<String>, String) {
        match p {
            [n] => (None, n.clone()),
            [s, n] => (Some(s.clone()), n.clone()),
            _ => (None, String::new()),
        }
    };
    match target {
        T::Column => {
            let (rel, col) = parts.split_at(parts.len().saturating_sub(1));
            let (s, t) = split(rel);
            cat.resolve_relation(s.as_deref(), &t, &path)
                .is_some_and(|r| r.column(&col[0]).is_some())
        }
        T::Table | T::View | T::MaterializedView => {
            let (s, t) = split(&parts);
            cat.resolve_relation(s.as_deref(), &t, &path).is_some()
        }
        T::Type | T::Domain => {
            let (s, t) = split(&parts);
            cat.resolve_type(s.as_deref(), &t, &path).is_some()
        }
        T::Function | T::Procedure => {
            let (s, t) = split(&parts);
            !cat.resolve_routines(s.as_deref(), &t, &path).is_empty()
        }
        T::Index => {
            let (s, t) = split(&parts);
            cat.indexes
                .contains_key(&QName::new(s.unwrap_or_else(|| "public".into()), t))
        }
        T::Sequence => {
            let (s, t) = split(&parts);
            cat.sequences
                .contains_key(&QName::new(s.unwrap_or_else(|| "public".into()), t))
        }
        T::Schema => cat.schemas.contains_key(&parts.join(".")),
        T::Extension => cat.extensions.contains_key(&parts.join(".")),
    }
}

fn diff_comments(old: &Catalog, new: &Catalog, rebuilt_views: &BTreeSet<QName>, out: &mut Out) {
    // A comment on a rebuilt view disappears with it.
    let on_rebuilt = |sql: &str| {
        comment_target(sql).is_some_and(|(_, parts)| {
            rebuilt_views.iter().any(|q| {
                parts.first() == Some(&q.name)
                    || (parts.len() >= 2 && parts[0] == q.schema && parts[1] == q.name)
            })
        })
    };
    for (key, sql) in &new.comments {
        if old.comments.get(key) != Some(sql) || on_rebuilt(sql) {
            out.push(COMMENT, sql.clone());
        }
    }
    for (key, sql) in &old.comments {
        if new.comments.contains_key(key) || !comment_object_exists(new, sql) {
            continue;
        }
        let Some(mut stmt) = pgzest_parser::parse(sql).ok().and_then(|mut v| v.pop()) else {
            continue;
        };
        if let Statement::Utility(u) = &mut stmt
            && let UtilityKind::Comment { text, .. } = &mut u.kind
        {
            *text = None;
            out.push(COMMENT, emit_statement(&stmt));
        }
    }
}
