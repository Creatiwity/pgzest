//! Query descriptor (PLAN.md §4, D7): the contract between analysis and all
//! targets (Rust macros, `.d.ts`, LSP). Stable, versioned JSON, ordered
//! deterministically.
//!
//! A descriptor is produced only if everything is established: a parameter
//! or a column of unknown type is an explicit refusal, never an assumed type.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use pgzest_ast::Statement;
use pgzest_ast::stmt::{InsertSource, SelectItem, SelectStmt};
use pgzest_catalog::ProjectCatalog;
use pgzest_catalog::model::{ConstraintKind, Relation};
use pgzest_diagnostics::{Diagnostic, Span, codes};
use pgzest_sema::{Analyzer, Dependency, Nullability, Ty, TypeSystem};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::file::{Cardinality, NamedQuery, Params};

/// Format version. Any incompatible change increments it.
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Descriptor {
    pub version: u32,
    pub name: String,
    /// SQL sent to the server: positional parameters.
    pub sql: String,
    pub command: Command,
    pub params: Vec<ParamDesc>,
    pub columns: Vec<ColumnDesc>,
    pub cardinality: Cardinality,
    /// Project objects the query depends on (relations, types, routines,
    /// operators), sorted; PostgreSQL built-in objects are not included, the
    /// server version pins them.
    pub depends_on: Vec<String>,
    /// Fingerprint of the normalized SQL and of the `depends_on` definitions.
    pub fingerprint: String,
    /// Data format of a `COPY … FROM STDIN`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy: Option<CopyDesc>,
    /// `-> other`: rows have the shape of the rows of query `other`, whose
    /// struct is reused (checked by [`check_row_reference`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row: Option<String>,
    /// Query with variants: `sql` is the reference variant.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dynamic: Option<DynamicDesc>,
}

/// Variants of a query (`[[ … ]]` blocks, `{{ … }}` choices).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DynamicDesc {
    /// Parameter deciding each block: low bits of the variant index, in this
    /// order (block 0 = bit 0).
    pub conditions: Vec<String>,
    /// Choices, next, in mixed radix.
    pub choices: Vec<ChoiceDesc>,
    /// SQL and parameters (in `$1…$n` order) of each variant, by index.
    pub variants: Vec<VariantDesc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChoiceDesc {
    pub name: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariantDesc {
    pub sql: String,
    pub params: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Select,
    Insert,
    Update,
    Delete,
    Call,
    /// `COPY table FROM STDIN`: `columns` are the loaded columns.
    CopyIn,
}

/// Format of a `COPY … FROM STDIN`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CopyDesc {
    /// Loaded table, qualified name.
    pub table: String,
    /// `text`, `csv` or `binary`.
    pub format: String,
    /// The first line (CSV) is a header.
    pub header: bool,
}

/// Catalog reference: qualified name, modifiers (`varchar(20)`), OID if
/// fixed by PostgreSQL (built-in types).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TypeDesc {
    pub pg: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oid: Option<u32>,
}

/// Nullability (D5): established non-null, possibly null, or unknown.
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
    /// Name (`id` for `$id`), or rank (`1` for `$1`).
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

/// Descriptor of a named query of a file. Diagnostics are positioned in the
/// file.
///
/// A query with variants (`[[ … ]]`, `{{ … }}`) is described variant by
/// variant; all must return the same columns.
pub fn describe_named(
    catalog: &ProjectCatalog,
    q: &NamedQuery,
) -> Result<Descriptor, Vec<Diagnostic>> {
    let Some(dynamic) = &q.dynamic else {
        let order: Option<Vec<String>> = match &q.params {
            Params::Named(list) => Some(list.iter().map(|p| format!("${}", p.name)).collect()),
            _ => None,
        };
        return describe_text(catalog, q, &q.analysis_text(), q.positional_sql(), order);
    };
    let selections = crate::dynamic::selections(&dynamic.structure);
    let reference = crate::file::reference_selection(&dynamic.structure);
    let mut variants = Vec::with_capacity(selections.len());
    let mut base: Option<Descriptor> = None;
    let mut described = Vec::new();
    for sel in &selections {
        let (text, sql, order) = q.variant(sel);
        let named_order = order.iter().map(|n| format!("${n}")).collect();
        let d = describe_text(catalog, q, &text, sql.clone(), Some(named_order)).map_err(|ds| {
            ds.into_iter()
                .map(|d| d.with_hint(format!("variant: {}", describe_selection(dynamic, sel))))
                .collect::<Vec<_>>()
        })?;
        if *sel == reference {
            base = Some(d.clone());
        }
        variants.push(VariantDesc { sql, params: order });
        described.push((sel, d));
    }
    let mut d = base.expect("the reference variant is one of the variants");
    let whole = Span {
        start: q.sql_range.start,
        end: q.sql_range.end,
    };
    for (sel, v) in &described {
        let same = v.columns.len() == d.columns.len()
            && v.columns
                .iter()
                .zip(&d.columns)
                .all(|(a, b)| a.name == b.name && a.ty == b.ty && a.nullable == b.nullable);
        if !same {
            return Err(vec![
                Diagnostic::error(
                    codes::QUERY_NOT_DESCRIBABLE,
                    format!(
                        "variant \"{}\" does not return the same columns as the others",
                        describe_selection(dynamic, sel)
                    ),
                )
                .with_span(whole.clone())
                .with_hint("all variants must return the same rows; a block or a choice changes the conditions, not the projection"),
            ]);
        }
        // The widest cardinality.
        d.cardinality = match (d.cardinality, v.cardinality) {
            (Cardinality::Many, _) | (_, Cardinality::Many) => Cardinality::Many,
            (Cardinality::Optional, _) | (_, Cardinality::Optional) => Cardinality::Optional,
            (c, _) => c,
        };
    }
    for p in d.params.iter_mut() {
        if dynamic.conditions.contains(&p.name) {
            p.nullable = NullabilityDesc::Nullable;
        }
    }
    d.dynamic = Some(DynamicDesc {
        conditions: dynamic.conditions.clone(),
        choices: dynamic
            .structure
            .choices
            .iter()
            .map(|c| ChoiceDesc {
                name: c.name.clone(),
                labels: c.alternatives.iter().map(|a| a.label.clone()).collect(),
            })
            .collect(),
        variants,
    });
    Ok(d)
}

/// `$status absent, sort = id`: a combination, for a message.
fn describe_selection(d: &crate::file::Dynamic, sel: &crate::dynamic::Selection) -> String {
    let blocks = d
        .conditions
        .iter()
        .zip(&sel.blocks)
        .map(|(c, on)| format!("${c} {}", if *on { "provided" } else { "absent" }));
    let choices = d
        .structure
        .choices
        .iter()
        .zip(&sel.choices)
        .map(|(c, k)| format!("{} = {}", c.name, c.alternatives[*k].label));
    blocks.chain(choices).collect::<Vec<_>>().join(", ")
}

/// Descriptor of an analyzed text of `q` (same length as its SQL): parse at
/// file positions, then analysis.
fn describe_text(
    catalog: &ProjectCatalog,
    q: &NamedQuery,
    analysis: &str,
    positional: String,
    order: Option<Vec<String>>,
) -> Result<Descriptor, Vec<Diagnostic>> {
    let whole = Span {
        start: q.sql_range.start,
        end: q.sql_range.end,
    };
    // Fragments inserted: the analyzed SQL is not the file text, the source
    // map brings each position back to it. Markers blanked (same length):
    // the parser reads SQL.
    let parsed = match &q.expanded {
        Some(e) => pgzest_parser::parse_mapped_with_outcome(analysis, &e.map),
        None => pgzest_parser::parse_at_with_outcome(analysis, q.sql_range.start),
    };
    let outcome = parsed.map_err(|e| {
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
                    "statement not read, the query is not described: {:?}",
                    sk.reason
                ),
            )
            .with_span(Span {
                start: sk.start,
                end: sk.end,
            }),
        ]);
    }
    let mut d = describe(
        catalog,
        &q.name,
        &outcome.statements,
        positional,
        order,
        q.cardinality,
        whole,
    )?;
    d.row = q.row.as_ref().map(|(r, _)| r.clone());
    // `$name?`: the value may be missing, whatever the analysis infers.
    if let Params::Named(list) = &q.params {
        for p in d.params.iter_mut() {
            if list.iter().any(|n| n.optional && n.name == p.name) {
                p.nullable = NullabilityDesc::Nullable;
            }
        }
    }
    Ok(d)
}

/// Can `d` reuse the row struct of the query it designates (`-> other`)?
/// Same columns, in the same order and of the same type; a column non-null
/// in the reference must be non-null here (the struct promises a value).
/// Never a conversion: any mismatch is an error.
///
/// `target`: the designated query, resolved by the caller (`None` if not
/// found).
pub fn check_row_reference(d: &Descriptor, target: Option<&Descriptor>) -> Result<(), String> {
    let Some(r) = &d.row else {
        return Ok(());
    };
    let Some(target) = target else {
        return Err(format!("reference query `{r}` not found"));
    };
    if let Some(next) = &target.row {
        return Err(format!(
            "`{r}` already reuses the rows of `{next}`: designate `{next}`"
        ));
    }
    if target.columns.is_empty() || target.copy.is_some() {
        return Err(format!("`{r}` returns no rows"));
    }
    if d.copy.is_some() || d.columns.is_empty() {
        return Err(format!(
            "`{}` returns no rows: `-> {r}` is irrelevant",
            d.name
        ));
    }
    if d.columns.len() != target.columns.len() {
        return Err(format!(
            "{} column(s), `{r}` has {} ({})",
            d.columns.len(),
            target.columns.len(),
            target
                .columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    let mut problems = Vec::new();
    for (i, (mine, theirs)) in d.columns.iter().zip(&target.columns).enumerate() {
        if mine.name != theirs.name {
            problems.push(format!(
                "column {}: `{}` where `{r}` has `{}`",
                i + 1,
                mine.name,
                theirs.name
            ));
            continue;
        }
        if mine.ty.pg != theirs.ty.pg {
            problems.push(format!(
                "`{}`: {} where `{r}` has {}",
                mine.name, mine.ty.pg, theirs.ty.pg
            ));
        }
        if theirs.nullable == NullabilityDesc::NonNull && mine.nullable != NullabilityDesc::NonNull
        {
            problems.push(format!("`{}` may be null here, not in `{r}`", mine.name));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "rows incompatible with `{r}`: {}",
            problems.join("; ")
        ))
    }
}

/// Descriptor of a SQL statement (`$n` or `$name`), named `name`.
pub fn describe_sql(
    catalog: &ProjectCatalog,
    name: &str,
    sql: &str,
) -> Result<Descriptor, Vec<Diagnostic>> {
    describe_sql_with(catalog, name, sql, &pgzest_macro::MacroSet::default())
}

/// Like [`describe_sql`], with the fragments of `macros` inserted.
pub fn describe_sql_with(
    catalog: &ProjectCatalog,
    name: &str,
    sql: &str,
    macros: &pgzest_macro::MacroSet,
) -> Result<Descriptor, Vec<Diagnostic>> {
    let header = format!("-- name: {name}\n");
    let file = crate::parse_query_file_with(&format!("{header}{sql}"), macros);
    let [q] = file.queries.as_slice() else {
        return Err(if file.diagnostics.is_empty() {
            vec![Diagnostic::error(
                codes::QUERY_EMPTY,
                "no query to describe",
            )]
        } else {
            file.diagnostics
        });
    };
    // Positions relative to the given text, without the added header.
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
                format!("{} statements: a query contains exactly one", stmts.len()),
            )
            .with_span(whole),
        ]);
    };
    let command = match stmt {
        Statement::Select(_) => Command::Select,
        Statement::Insert(_) => Command::Insert,
        Statement::Update(_) => Command::Update,
        Statement::Delete(_) => Command::Delete,
        Statement::Utility(u) if matches!(u.kind, pgzest_ast::UtilityKind::Call { .. }) => {
            Command::Call
        }
        Statement::Utility(u) if copy_in(&u.kind).is_some() => Command::CopyIn,
        _ => {
            return Err(vec![
                Diagnostic::error(
                    codes::QUERY_NOT_DESCRIBABLE,
                    "only SELECT, INSERT, UPDATE, DELETE, CALL and COPY … FROM STDIN are described",
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
                .map(|columns| pgzest_sema::Description {
                    columns,
                    params: Vec::new(),
                })
        }
        None => a.describe(stmt),
    }
    .map_err(|e| {
        let span = if e.span == pgzest_ast::Span::DUMMY {
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
    let mut ordered: Vec<&pgzest_sema::Param> = described.params.iter().collect();
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
            Some(ty) if *ty != pgzest_sema::pg("unknown") => params.push(ParamDesc {
                name: pname,
                ty: type_desc(ty, Vec::new()),
                nullable: p.nullable.into(),
            }),
            _ => errors.push(
                Diagnostic::error(
                    codes::QUERY_PARAMETER_UNTYPED,
                    format!(
                        "the type of parameter {} is not established by its usage",
                        p.name
                    ),
                )
                .with_span(whole.clone())
                .with_hint(format!("specify the type with a cast: {}::type", p.name)),
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
        row: None,
        dynamic: None,
    })
}

/// `COPY table [(columns)] FROM STDIN [(options)]`: table, columns, format.
fn copy_in(
    u: &pgzest_ast::UtilityKind,
) -> Option<(
    &pgzest_ast::ObjectName,
    &[pgzest_ast::expr::Ident],
    CopyDesc,
)> {
    use pgzest_ast::utility::{CopyOption, CopySource, CopyTarget};
    let pgzest_ast::UtilityKind::Copy(c) = u else {
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

/// A declaration (`-- name: x :one`) narrows the inference without
/// contradicting it: `none` requires a query returning no rows, and vice
/// versa. Narrowing `many` to `one` or `optional` is a contract checked at
/// run time.
fn check_declared(declared: Cardinality, inferred: Cardinality) -> Result<(), String> {
    match (declared, inferred) {
        (Cardinality::None, Cardinality::None) => Ok(()),
        (Cardinality::None, i) => Err(format!(
            "declared `none`, but the query returns rows (inferred: `{}`)",
            i.as_str()
        )),
        (d, Cardinality::None) => Err(format!(
            "declared `{}`, but the query returns no rows (no RETURNING)",
            d.as_str()
        )),
        _ => Ok(()),
    }
}

/// Cardinality inferred conservatively (PLAN.md §4); `many` as soon as no
/// safe rule applies.
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
            // Unconditional `ON CONFLICT DO UPDATE`: the inserted or updated
            // row is always returned.
            (
                InsertSource::Values(rows),
                Some(pgzest_ast::stmt::OnConflict {
                    action:
                        pgzest_ast::stmt::OnConflictAction::Update {
                            where_clause: None, ..
                        },
                    ..
                }),
            ) if rows.len() == 1 => Cardinality::One,
            // `DO NOTHING` / `DO UPDATE … WHERE`: the row may not be
            // returned.
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
        // `CALL` of a procedure with `INOUT` arguments: one row.
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
    // Aggregate without GROUP BY: exactly one row (`HAVING` may discard it).
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
        && let pgzest_ast::stmt::TableSource::Table(name) = &t.source
        && unique_key_fixed(catalog, a, name, t.alias.as_ref(), s.where_clause.as_ref())
    {
        Cardinality::Optional
    } else {
        Cardinality::Many
    };
    // `LIMIT 1`: at most one row.
    let limit_one = matches!(
        s.limit.as_ref().map(|l| &l.kind),
        Some(pgzest_ast::expr::ExprKind::Literal(pgzest_ast::expr::Literal::Number(n))) if n == "1"
    );
    match base {
        Cardinality::Many if limit_one => Cardinality::Optional,
        c => c,
    }
}

/// Set-returning functions in the `SELECT` list (one row per element).
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
                        .any(|r| r.kind == pgzest_catalog::model::RoutineKind::Aggregate)
                {
                    found = true;
                }
            });
        }
    }
    found
}

fn split(name: &pgzest_ast::ObjectName) -> (Option<&str>, &str) {
    match name.0.as_slice() {
        [s, n] => (Some(s.value.as_str()), n.value.as_str()),
        [.., n] => (None, n.value.as_str()),
        [] => (None, ""),
    }
}

/// Function calls of an expression, excluding subqueries: (name, windowed).
fn visit_calls(e: &pgzest_ast::Expr, f: &mut dyn FnMut(&pgzest_ast::ObjectName, bool)) {
    use pgzest_ast::expr::ExprKind;
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

/// `WHERE` fixes all columns of a primary or unique key of the table through
/// conjunctive equalities (`col = value` without reference to the table):
/// at most one row.
fn unique_key_fixed(
    catalog: &ProjectCatalog,
    a: &Analyzer,
    table: &pgzest_ast::ObjectName,
    alias: Option<&pgzest_ast::expr::Ident>,
    where_clause: Option<&pgzest_ast::Expr>,
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

/// Unique keys of a table: primary key, `UNIQUE` constraints, and unique
/// indexes on plain columns, without a predicate (this is how Prisma sets
/// its `@unique`). A partial or expression index guarantees nothing for an
/// equality on its columns.
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
        let Ok(stmts) = pgzest_parser::parse(&index.definition) else {
            continue;
        };
        let [Statement::Ddl(pgzest_ast::DdlStatement::CreateIndex(ci))] = stmts.as_slice() else {
            continue;
        };
        if ci.where_clause.is_some() {
            continue;
        }
        let columns: Option<Vec<String>> = ci
            .elements
            .iter()
            .map(|e| match &e.element {
                pgzest_ast::ddl::IndexElement::Column(c) => Some(c.value.clone()),
                pgzest_ast::ddl::IndexElement::Expr(_) => None,
            })
            .collect();
        if let Some(columns) = columns {
            keys.push(columns);
        }
    }
    keys
}

/// Qualified name of a descriptor (`schema.name`, identifiers quoted when
/// needed: `public."User"`).
pub fn parse_qualified(s: &str) -> Option<pgzest_catalog::QName> {
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
        [schema, name] => Some(pgzest_catalog::QName::new(schema, name)),
        _ => None,
    }
}

/// Table columns fixed by `col = expr` in a top-level conjunction, `expr`
/// referencing no column.
fn equalities(e: &pgzest_ast::Expr, qualifier: &str, out: &mut BTreeSet<String>) {
    use pgzest_ast::expr::{BinaryOp, ExprKind};
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

fn references_columns(e: &pgzest_ast::Expr) -> bool {
    use pgzest_ast::expr::ExprKind;
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

/// SHA-256 of the normalized SQL (re-emitted from the AST: formatting and
/// comments do not count) and of the dependency definitions.
fn fingerprint(catalog: &ProjectCatalog, stmt: &Statement, deps: &BTreeSet<Dependency>) -> String {
    let mut h = Sha256::new();
    h.update(format!("pgzest-descriptor/{VERSION}\n").as_bytes());
    h.update(pgzest_ast::emit::emit_statement(stmt).as_bytes());
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

/// Writes `d` to the cache (`<dir>/<fingerprint>.json`).
pub fn write_cache(dir: &Path, d: &Descriptor) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", d.fingerprint));
    let json = serde_json::to_string_pretty(d).map_err(std::io::Error::other)?;
    std::fs::write(&path, json + "\n")?;
    Ok(path)
}

/// Cached descriptor for this fingerprint, if it exists and has this format
/// version.
pub fn read_cache(dir: &Path, fingerprint: &str) -> Option<Descriptor> {
    let text = std::fs::read_to_string(dir.join(format!("{fingerprint}.json"))).ok()?;
    serde_json::from_str::<Descriptor>(&text)
        .ok()
        .filter(|d| d.version == VERSION)
}

// ── Modules ───────────────────────────────────────────────────────────────────

/// Queries of a file and the module receiving them: path from the root of
/// the generated code (`["order", "book"]` for `order/book.sql`), empty for
/// the root.
#[derive(Debug, Clone)]
pub struct QueryModule {
    pub path: Vec<String>,
    pub descriptors: Vec<Descriptor>,
    /// Source file (readable label, absolute path) and header line of each
    /// query: the generated code links back to it.
    pub source: Option<QuerySource>,
}

/// Where a generated query is written.
#[derive(Debug, Clone, Default)]
pub struct QuerySource {
    /// Displayed path (`queries/menu.sql`).
    pub label: String,
    /// Absolute path, for the link.
    pub path: std::path::PathBuf,
    /// Line (1-based) of the `-- name:` header of each query.
    pub lines: std::collections::BTreeMap<String, usize>,
}

/// Query designated by `-> r` from module `from`: `name` in the same module,
/// `a::b::name` from the root of the generated code.
pub fn resolve_row<'m>(
    from: &[String],
    r: &str,
    modules: &'m [QueryModule],
) -> Option<(&'m [String], &'m Descriptor)> {
    let parts: Vec<&str> = r.split("::").collect();
    let (path, name): (Vec<String>, &str) = match parts.split_last() {
        Some((name, [])) => (from.to_vec(), name),
        Some((name, path)) => (path.iter().map(|s| s.to_string()).collect(), name),
        None => return None,
    };
    let m = modules.iter().find(|m| m.path == path)?;
    let d = m.descriptors.iter().find(|d| d.name == name)?;
    Some((&m.path, d))
}
