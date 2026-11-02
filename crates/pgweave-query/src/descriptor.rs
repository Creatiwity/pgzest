//! Descripteur de requête (PLAN.md §4, D7) : le contrat entre l'analyse et
//! toutes les cibles (macros Rust, `.d.ts`, LSP). JSON stable, versionné,
//! ordonné de façon déterministe.
//!
//! Un descripteur n'est produit que si tout est établi : un paramètre ou une
//! colonne de type inconnu est un refus explicite, jamais un type supposé.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use pgweave_ast::Statement;
use pgweave_ast::stmt::{InsertSource, SelectItem, SelectStmt};
use pgweave_catalog::ProjectCatalog;
use pgweave_catalog::model::{ConstraintKind, Relation};
use pgweave_diagnostics::{Diagnostic, Span, codes};
use pgweave_sema::{Analyzer, Dependency, Nullability, Ty, TypeSystem};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::file::{Cardinality, NamedQuery, Params};

/// Version du format. Tout changement incompatible l'incrémente.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Descriptor {
    pub version: u32,
    pub name: String,
    /// SQL envoyé au serveur : paramètres positionnels.
    pub sql: String,
    pub command: Command,
    pub params: Vec<ParamDesc>,
    pub columns: Vec<ColumnDesc>,
    pub cardinality: Cardinality,
    /// Objets du projet dont dépend la requête (relations, types, routines,
    /// opérateurs), triés ; les objets intégrés à PostgreSQL n'y figurent
    /// pas, la version du serveur les fixe.
    pub depends_on: Vec<String>,
    /// Empreinte du SQL normalisé et des définitions de `depends_on`.
    pub fingerprint: String,
    /// Format des données d'un `COPY … FROM STDIN`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy: Option<CopyDesc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Select,
    Insert,
    Update,
    Delete,
    Call,
    /// `COPY table FROM STDIN` : `columns` sont les colonnes alimentées.
    CopyIn,
}

/// Format d'un `COPY … FROM STDIN`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CopyDesc {
    /// Table alimentée, nom qualifié.
    pub table: String,
    /// `text`, `csv` ou `binary`.
    pub format: String,
    /// La première ligne (CSV) est un en-tête.
    pub header: bool,
}

/// Référence au catalogue : nom qualifié, modificateurs (`varchar(20)`),
/// OID s'il est fixé par PostgreSQL (types intégrés).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypeDesc {
    pub pg: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<u32>,
}

/// Nullabilité (D5) : établie non nulle, possiblement nulle, ou inconnue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NullabilityDesc {
    NonNull,
    Nullable,
    Unknown,
}

impl From<Nullability> for NullabilityDesc {
    fn from(n: Nullability) -> Self {
        match n {
            Nullability::NonNull => NullabilityDesc::NonNull,
            Nullability::Nullable => NullabilityDesc::Nullable,
            Nullability::Unknown => NullabilityDesc::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParamDesc {
    /// Nom (`id` pour `$id`), ou rang (`1` pour `$1`).
    pub name: String,
    #[serde(rename = "type")]
    pub ty: TypeDesc,
    pub nullable: NullabilityDesc,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnDesc {
    pub name: String,
    #[serde(rename = "type")]
    pub ty: TypeDesc,
    pub nullable: NullabilityDesc,
}

/// Descripteur d'une requête nommée d'un fichier. Les diagnostics sont
/// positionnés dans le fichier.
pub fn describe_named(
    catalog: &ProjectCatalog,
    q: &NamedQuery,
) -> Result<Descriptor, Vec<Diagnostic>> {
    let whole = Span {
        start: q.sql_range.start,
        end: q.sql_range.end,
    };
    let outcome =
        pgweave_parser::parse_at_with_outcome(&q.sql, q.sql_range.start).map_err(|e| {
            vec![
                Diagnostic::error(codes::PARSE_ERROR, format!("parse error: {e}"))
                    .with_span(whole.clone()),
            ]
        })?;
    if let Some(sk) = outcome.skipped.first() {
        return Err(vec![
            Diagnostic::error(
                codes::UNPARSABLE_STATEMENT,
                format!(
                    "statement non lu, la requête n'est pas décrite : {:?}",
                    sk.reason
                ),
            )
            .with_span(Span {
                start: sk.start,
                end: sk.end,
            }),
        ]);
    }
    // Ordre des paramètres : celui de la réécriture positionnelle, c'est-à-
    // dire de première apparition dans le texte (et non l'ordre d'analyse).
    let order: Option<Vec<String>> = match &q.params {
        Params::Named(list) => Some(list.iter().map(|p| format!("${}", p.name)).collect()),
        _ => None,
    };
    describe(
        catalog,
        &q.name,
        &outcome.statements,
        q.positional_sql(),
        order,
        q.cardinality,
        whole,
    )
}

/// Descripteur d'un statement SQL (`$n` ou `$name`), nommé `name`.
pub fn describe_sql(
    catalog: &ProjectCatalog,
    name: &str,
    sql: &str,
) -> Result<Descriptor, Vec<Diagnostic>> {
    let header = format!("-- name: {name}\n");
    let file = crate::parse_query_file(&format!("{header}{sql}"));
    let [q] = file.queries.as_slice() else {
        return Err(if file.diagnostics.is_empty() {
            vec![Diagnostic::error(
                codes::QUERY_EMPTY,
                "aucune requête à décrire",
            )]
        } else {
            file.diagnostics
        });
    };
    // Positions rapportées au texte donné, sans l'en-tête ajouté.
    describe_named(catalog, q).map_err(|ds| {
        ds.into_iter()
            .map(|mut d| {
                if let Some(s) = d.span.as_mut() {
                    s.start = s.start.saturating_sub(header.len());
                    s.end = s.end.saturating_sub(header.len());
                }
                d
            })
            .collect()
    })
}

fn describe(
    catalog: &ProjectCatalog,
    name: &str,
    stmts: &[Statement],
    positional_sql: String,
    order: Option<Vec<String>>,
    declared: Option<Cardinality>,
    whole: Span,
) -> Result<Descriptor, Vec<Diagnostic>> {
    let at = |start: usize, end: usize| Span { start, end };
    let [stmt] = stmts else {
        return Err(vec![
            Diagnostic::error(
                codes::QUERY_MULTIPLE_STATEMENTS,
                format!(
                    "{} statements : une requête en contient exactement un",
                    stmts.len()
                ),
            )
            .with_span(whole),
        ]);
    };
    let command = match stmt {
        Statement::Select(_) => Command::Select,
        Statement::Insert(_) => Command::Insert,
        Statement::Update(_) => Command::Update,
        Statement::Delete(_) => Command::Delete,
        Statement::Utility(u) if matches!(u.kind, pgweave_ast::UtilityKind::Call { .. }) => {
            Command::Call
        }
        Statement::Utility(u) if copy_in(&u.kind).is_some() => Command::CopyIn,
        _ => {
            return Err(vec![
                Diagnostic::error(
                    codes::QUERY_NOT_DESCRIBABLE,
                    "seuls SELECT, INSERT, UPDATE, DELETE, CALL et COPY … FROM STDIN sont décrits",
                )
                .with_span(whole),
            ]);
        }
    };

    let ts = TypeSystem::new(&catalog.catalog);
    let mut a = Analyzer::new(ts, catalog.search_path.clone());
    let copy = match stmt {
        Statement::Utility(u) => copy_in(&u.kind),
        _ => None,
    };
    let described = match &copy {
        Some((table, cols, _)) => {
            a.copy_columns(table, cols)
                .map(|columns| pgweave_sema::Description {
                    columns,
                    params: Vec::new(),
                })
        }
        None => a.describe(stmt),
    }
    .map_err(|e| {
        let span = if e.span == pgweave_ast::Span::DUMMY {
            whole.clone()
        } else {
            at(e.span.start as usize, e.span.end as usize)
        };
        let mut d = Diagnostic::error(e.code, e.message).with_span(span);
        d.hint = e.hint;
        vec![d]
    })?;

    let mut errors = Vec::new();
    let mut types: BTreeSet<Dependency> = BTreeSet::new();
    let mut type_desc = |ty: &Ty, modifiers: Vec<String>| {
        let def = a.ts.def(ty);
        if !matches!(ty.schema.as_str(), "pg_catalog" | "information_schema") {
            types.insert(Dependency::Type(ty.clone()));
        }
        TypeDesc {
            pg: ty.to_string(),
            modifiers,
            oid: def.and_then(|d| d.oid),
        }
    };
    let mut ordered: Vec<&pgweave_sema::Param> = described.params.iter().collect();
    if let Some(order) = &order {
        ordered.sort_by_key(|p| {
            order
                .iter()
                .position(|n| *n == p.name)
                .unwrap_or(usize::MAX)
        });
    }
    let mut params = Vec::new();
    for p in ordered {
        let pname = p.name.trim_start_matches('$').to_string();
        match &p.ty {
            Some(ty) if *ty != pgweave_sema::pg("unknown") => params.push(ParamDesc {
                name: pname,
                ty: type_desc(ty, Vec::new()),
                nullable: p.nullable.into(),
            }),
            _ => errors.push(
                Diagnostic::error(
                    codes::QUERY_PARAMETER_UNTYPED,
                    format!(
                        "le type du paramètre {} n'est pas établi par son usage",
                        p.name
                    ),
                )
                .with_span(whole.clone())
                .with_hint(format!("préciser le type par un cast : {}::type", p.name)),
            ),
        }
    }
    let columns: Vec<ColumnDesc> = described
        .columns
        .iter()
        .map(|c| ColumnDesc {
            name: c.name.clone(),
            ty: type_desc(&c.ty, c.modifiers.clone()),
            nullable: c.nullable.into(),
        })
        .collect();
    if !errors.is_empty() {
        return Err(errors);
    }

    let inferred = if copy.is_some() {
        Cardinality::None
    } else {
        infer_cardinality(catalog, stmt, &a, columns.is_empty())
    };
    let cardinality = match declared {
        None => inferred,
        Some(d) => {
            check_declared(d, inferred).map_err(|msg| {
                vec![
                    Diagnostic::error(codes::QUERY_CARDINALITY_MISMATCH, msg)
                        .with_span(whole.clone()),
                ]
            })?;
            d
        }
    };

    let deps: BTreeSet<Dependency> =
        a.ts.depends
            .borrow()
            .iter()
            .cloned()
            .chain(types)
            .filter(|d| !d.is_builtin())
            .collect();
    let mut depends_on: Vec<String> = deps.iter().map(ToString::to_string).collect();
    depends_on.sort();
    let fingerprint = fingerprint(catalog, stmt, &deps);

    Ok(Descriptor {
        version: VERSION,
        name: name.to_string(),
        sql: positional_sql,
        command,
        params,
        columns,
        cardinality,
        depends_on,
        fingerprint,
        copy: copy.map(|(table, _, mut c)| {
            let (schema, n) = split(table);
            c.table = catalog
                .catalog
                .resolve_relation(schema, n, &catalog.search_path)
                .map(|r| r.name.to_string())
                .unwrap_or_default();
            c
        }),
    })
}

/// `COPY table [(colonnes)] FROM STDIN [(options)]` : table, colonnes,
/// format.
fn copy_in(
    u: &pgweave_ast::UtilityKind,
) -> Option<(
    &pgweave_ast::ObjectName,
    &[pgweave_ast::expr::Ident],
    CopyDesc,
)> {
    use pgweave_ast::utility::{CopyOption, CopySource, CopyTarget};
    let pgweave_ast::UtilityKind::Copy(c) = u else {
        return None;
    };
    let (false, CopyTarget::Stdin, CopySource::Table { name, columns }) =
        (c.to, &c.target, &c.source)
    else {
        return None;
    };
    let mut desc = CopyDesc {
        table: String::new(),
        format: "text".into(),
        header: false,
    };
    for o in &c.options {
        match o {
            CopyOption::Format(f) => desc.format = f.to_ascii_lowercase(),
            CopyOption::Header(h) => desc.header = *h,
            _ => {}
        }
    }
    Some((name, columns.as_slice(), desc))
}

/// Une déclaration (`-- name: x :one`) restreint l'inférence, sans la
/// contredire : `none` exige une requête sans ligne renvoyée, et
/// inversement. Restreindre `many` en `one` ou `optional` est un contrat
/// vérifié à l'exécution.
fn check_declared(declared: Cardinality, inferred: Cardinality) -> Result<(), String> {
    match (declared, inferred) {
        (Cardinality::None, Cardinality::None) => Ok(()),
        (Cardinality::None, i) => Err(format!(
            "déclarée `none`, mais la requête renvoie des lignes (inférée : `{}`)",
            i.as_str()
        )),
        (d, Cardinality::None) => Err(format!(
            "déclarée `{}`, mais la requête ne renvoie aucune ligne (pas de RETURNING)",
            d.as_str()
        )),
        _ => Ok(()),
    }
}

/// Cardinalité inférée de façon conservatrice (PLAN.md §4) ; `many` dès
/// qu'une règle sûre ne s'applique pas.
fn infer_cardinality(
    catalog: &ProjectCatalog,
    stmt: &Statement,
    a: &Analyzer,
    no_columns: bool,
) -> Cardinality {
    if no_columns {
        return Cardinality::None;
    }
    match stmt {
        Statement::Select(s) => select_cardinality(catalog, s, a),
        Statement::Insert(i) => match (&i.source, &i.on_conflict) {
            (InsertSource::Values(rows), None) if rows.len() == 1 => Cardinality::One,
            (InsertSource::DefaultValues, None) => Cardinality::One,
            // `ON CONFLICT DO UPDATE` sans condition : la ligne insérée ou
            // mise à jour est toujours renvoyée.
            (
                InsertSource::Values(rows),
                Some(pgweave_ast::stmt::OnConflict {
                    action:
                        pgweave_ast::stmt::OnConflictAction::Update {
                            where_clause: None, ..
                        },
                    ..
                }),
            ) if rows.len() == 1 => Cardinality::One,
            // `DO NOTHING` / `DO UPDATE … WHERE` : la ligne peut ne pas être
            // renvoyée.
            (InsertSource::Values(rows), Some(_)) if rows.len() == 1 => Cardinality::Optional,
            _ => Cardinality::Many,
        },
        Statement::Update(u) => {
            if u.from.is_empty()
                && u.joins.is_empty()
                && unique_key_fixed(
                    catalog,
                    a,
                    &u.table.name,
                    u.table.alias.as_ref(),
                    u.where_clause.as_ref(),
                )
            {
                Cardinality::Optional
            } else {
                Cardinality::Many
            }
        }
        Statement::Delete(d) => {
            if d.using.is_empty()
                && d.joins.is_empty()
                && unique_key_fixed(
                    catalog,
                    a,
                    &d.table.name,
                    d.table.alias.as_ref(),
                    d.where_clause.as_ref(),
                )
            {
                Cardinality::Optional
            } else {
                Cardinality::Many
            }
        }
        // `CALL` d'une procédure à arguments `INOUT` : une ligne.
        _ => Cardinality::One,
    }
}

fn select_cardinality(catalog: &ProjectCatalog, s: &SelectStmt, a: &Analyzer) -> Cardinality {
    if !s.set_ops.is_empty() || !s.with.is_empty() || returns_sets(s, a) {
        return Cardinality::Many;
    }
    if !s.values.is_empty() {
        return match s.values.len() {
            1 => Cardinality::One,
            _ => Cardinality::Many,
        };
    }
    let filtered = s.where_clause.is_some() || s.having.is_some();
    let limited = s.offset.is_some();
    // Agrégat sans GROUP BY : exactement une ligne (`HAVING` peut l'écarter).
    let aggregate = s.group_by.is_empty() && has_aggregate(s, a);
    let base = if s.from.is_empty() && s.joins.is_empty() {
        if filtered || limited {
            Cardinality::Optional
        } else {
            Cardinality::One
        }
    } else if aggregate {
        if s.having.is_some() || limited {
            Cardinality::Optional
        } else {
            Cardinality::One
        }
    } else if s.joins.is_empty()
        && s.group_by.is_empty()
        && let [t] = s.from.as_slice()
        && let pgweave_ast::stmt::TableSource::Table(name) = &t.source
        && unique_key_fixed(catalog, a, name, t.alias.as_ref(), s.where_clause.as_ref())
    {
        Cardinality::Optional
    } else {
        Cardinality::Many
    };
    // `LIMIT 1` : au plus une ligne.
    let limit_one = matches!(
        s.limit.as_ref().map(|l| &l.kind),
        Some(pgweave_ast::expr::ExprKind::Literal(pgweave_ast::expr::Literal::Number(n))) if n == "1"
    );
    match base {
        Cardinality::Many if limit_one => Cardinality::Optional,
        c => c,
    }
}

/// Fonctions renvoyant un ensemble dans la liste `SELECT` (une ligne par
/// élément).
fn returns_sets(s: &SelectStmt, a: &Analyzer) -> bool {
    let mut found = false;
    for item in &s.columns {
        if let SelectItem::Expr { expr, .. } = item {
            visit_calls(expr, &mut |name, _| {
                let (schema, n) = split(name);
                if a.ts
                    .catalog
                    .resolve_routines(schema, n, &a.path)
                    .iter()
                    .any(|r| r.returns_set)
                {
                    found = true;
                }
            });
        }
    }
    found
}

fn has_aggregate(s: &SelectStmt, a: &Analyzer) -> bool {
    let mut found = false;
    for item in &s.columns {
        if let SelectItem::Expr { expr, .. } = item {
            visit_calls(expr, &mut |name, windowed| {
                let (schema, n) = split(name);
                if !windowed
                    && a.ts
                        .catalog
                        .resolve_routines(schema, n, &a.path)
                        .iter()
                        .any(|r| r.kind == pgweave_catalog::model::RoutineKind::Aggregate)
                {
                    found = true;
                }
            });
        }
    }
    found
}

fn split(name: &pgweave_ast::ObjectName) -> (Option<&str>, &str) {
    match name.0.as_slice() {
        [s, n] => (Some(s.value.as_str()), n.value.as_str()),
        [.., n] => (None, n.value.as_str()),
        [] => (None, ""),
    }
}

/// Appels de fonction d'une expression, hors sous-requêtes : (nom, fenêtré).
fn visit_calls(e: &pgweave_ast::Expr, f: &mut dyn FnMut(&pgweave_ast::ObjectName, bool)) {
    use pgweave_ast::expr::ExprKind;
    if let ExprKind::FunctionCall { name, over, .. } = &e.kind {
        f(name, over.is_some());
    }
    if matches!(
        e.kind,
        ExprKind::Subquery(_) | ExprKind::Exists { .. } | ExprKind::InSubquery { .. }
    ) {
        return;
    }
    for c in e.children() {
        visit_calls(c, f);
    }
}

/// `WHERE` fixe toutes les colonnes d'une clé primaire ou unique de la
/// table par des égalités en conjonction (`col = valeur` sans référence à
/// la table) : au plus une ligne.
fn unique_key_fixed(
    catalog: &ProjectCatalog,
    a: &Analyzer,
    table: &pgweave_ast::ObjectName,
    alias: Option<&pgweave_ast::expr::Ident>,
    where_clause: Option<&pgweave_ast::Expr>,
) -> bool {
    let Some(w) = where_clause else {
        return false;
    };
    let (schema, n) = split(table);
    let Some(rel) = catalog.catalog.resolve_relation(schema, n, &a.path) else {
        return false;
    };
    let qualifier = alias.map_or(n, |a| a.value.as_str());
    let mut fixed = BTreeSet::new();
    equalities(w, qualifier, &mut fixed);
    unique_keys(catalog, rel)
        .iter()
        .any(|key| !key.is_empty() && key.iter().all(|c| fixed.contains(c)))
}

/// Clés uniques d'une table : clé primaire, contraintes `UNIQUE`, et index
/// uniques sur de simples colonnes, sans prédicat (Prisma pose ainsi ses
/// `@unique`). Un index partiel ou sur expression ne garantit rien pour une
/// égalité sur ses colonnes.
fn unique_keys(catalog: &ProjectCatalog, rel: &Relation) -> Vec<Vec<String>> {
    let mut keys: Vec<Vec<String>> = rel
        .constraints
        .iter()
        .filter(|c| !c.not_valid)
        .filter_map(|c| match &c.kind {
            ConstraintKind::PrimaryKey { columns } | ConstraintKind::Unique { columns } => {
                Some(columns.clone())
            }
            _ => None,
        })
        .collect();
    for index in catalog
        .catalog
        .indexes
        .values()
        .filter(|i| i.unique && i.table == rel.name)
    {
        let Ok(stmts) = pgweave_parser::parse(&index.definition) else {
            continue;
        };
        let [Statement::Ddl(pgweave_ast::DdlStatement::CreateIndex(ci))] = stmts.as_slice() else {
            continue;
        };
        if ci.where_clause.is_some() {
            continue;
        }
        let columns: Option<Vec<String>> = ci
            .elements
            .iter()
            .map(|e| match e {
                pgweave_ast::ddl::IndexElement::Column(c) => Some(c.value.clone()),
                pgweave_ast::ddl::IndexElement::Expr(_) => None,
            })
            .collect();
        if let Some(columns) = columns {
            keys.push(columns);
        }
    }
    keys
}

/// Nom qualifié d'un descripteur (`schéma.nom`, identifiants quotés au
/// besoin : `public."User"`).
pub fn parse_qualified(s: &str) -> Option<pgweave_catalog::QName> {
    let mut parts = Vec::new();
    let mut chars = s.chars().peekable();
    let mut current = String::new();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                while let Some(c) = chars.next() {
                    if c == '"' {
                        if chars.peek() == Some(&'"') {
                            chars.next();
                            current.push('"');
                        } else {
                            break;
                        }
                    } else {
                        current.push(c);
                    }
                }
            }
            '.' => parts.push(std::mem::take(&mut current)),
            c => current.push(c),
        }
    }
    parts.push(current);
    match parts.as_slice() {
        [schema, name] => Some(pgweave_catalog::QName::new(schema, name)),
        _ => None,
    }
}

/// Colonnes de la table fixées par `col = expr` dans une conjonction de
/// premier niveau, `expr` ne citant aucune colonne.
fn equalities(e: &pgweave_ast::Expr, qualifier: &str, out: &mut BTreeSet<String>) {
    use pgweave_ast::expr::{BinaryOp, ExprKind};
    match &e.kind {
        ExprKind::BinaryOp {
            left,
            op: BinaryOp::And,
            right,
        } => {
            equalities(left, qualifier, out);
            equalities(right, qualifier, out);
        }
        ExprKind::BinaryOp {
            left,
            op: BinaryOp::Eq,
            right,
        } => {
            for (col, value) in [(left, right), (right, left)] {
                if let ExprKind::Column(c) = &col.kind
                    && c.table.as_ref().is_none_or(|t| t.value == qualifier)
                    && !references_columns(value)
                {
                    out.insert(c.column.value.clone());
                }
            }
        }
        _ => {}
    }
}

fn references_columns(e: &pgweave_ast::Expr) -> bool {
    use pgweave_ast::expr::ExprKind;
    match &e.kind {
        ExprKind::Column(_)
        | ExprKind::Subquery(_)
        | ExprKind::Exists { .. }
        | ExprKind::InSubquery { .. }
        | ExprKind::Wildcard
        | ExprKind::QualifiedWildcard(_) => true,
        _ => e.children().into_iter().any(references_columns),
    }
}

/// SHA-256 du SQL normalisé (réémis depuis l'AST : la mise en forme et les
/// commentaires n'y comptent pas) et des définitions des dépendances.
fn fingerprint(catalog: &ProjectCatalog, stmt: &Statement, deps: &BTreeSet<Dependency>) -> String {
    let mut h = Sha256::new();
    h.update(format!("pgweave-descriptor/{VERSION}\n").as_bytes());
    h.update(pgweave_ast::emit::emit_statement(stmt).as_bytes());
    let cat = &catalog.catalog;
    for d in deps {
        h.update(b"\n");
        h.update(d.to_string().as_bytes());
        h.update(b"=");
        let definition = match d {
            Dependency::Relation(q) => cat
                .relations
                .get(q)
                .and_then(|r| serde_json::to_string(r).ok()),
            Dependency::Type(q) => cat.types.get(q).and_then(|t| serde_json::to_string(t).ok()),
            Dependency::Routine(q, _) => cat
                .routines
                .get(q)
                .and_then(|r| serde_json::to_string(r).ok()),
            Dependency::Operator(q, _, _) => cat
                .operators
                .get(q)
                .and_then(|o| serde_json::to_string(o).ok()),
        };
        h.update(definition.unwrap_or_default().as_bytes());
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Écrit `d` dans le cache (`<dir>/<fingerprint>.json`).
pub fn write_cache(dir: &Path, d: &Descriptor) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", d.fingerprint));
    let json = serde_json::to_string_pretty(d).map_err(std::io::Error::other)?;
    std::fs::write(&path, json + "\n")?;
    Ok(path)
}

/// Descripteur en cache pour cette empreinte, s'il existe et est de cette
/// version du format.
pub fn read_cache(dir: &Path, fingerprint: &str) -> Option<Descriptor> {
    let text = std::fs::read_to_string(dir.join(format!("{fingerprint}.json"))).ok()?;
    serde_json::from_str::<Descriptor>(&text)
        .ok()
        .filter(|d| d.version == VERSION)
}
