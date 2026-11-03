use pgweave_catalog::project::ProjectBuilder;
use pgweave_parser::{SkipReason, SkippedStatement};
use pgweave_sema::{Ty, TypeSystem, ViewTyper};

use crate::case::Case;

/// What pgweave claims about a query. `None` = pgweave does not say
/// ("not inferred"), which is distinct from an empty list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OurDescription {
    pub params: Option<Vec<String>>,
    /// Name (if pgweave infers one) and type of each output column.
    pub columns: Option<Vec<(Option<String>, String)>>,
    /// What degraded the analysis: schema or query statements skipped by
    /// the parser. Empty = complete analysis.
    pub notes: Vec<String>,
}

/// Describes each query of the case through its descriptor (PLAN.md §4), as
/// a code generator consumes it.
pub fn describe_ours(case: &Case) -> Vec<OurDescription> {
    // The resolver does not say what it skipped: reparse to find out.
    // A table missing from the index explains "wrong" columns downstream,
    // so the report must show it.
    let schema_notes: Vec<String> = match pgweave_parser::parse_with_outcome(&case.schema) {
        Ok(o) => o
            .skipped
            .iter()
            .map(|s| format!("schema: {}", describe_skip(s)))
            .collect(),
        Err(e) => vec![format!("unreadable schema: {e}")],
    };
    // Schema replay, views typed by semantic analysis.
    let schema = {
        let mut b = ProjectBuilder::new().with_typer(Some(ViewTyper::shared()));
        let parsed = pgweave_parser::parse_with_outcome(&case.schema);
        b.add_file(
            std::path::Path::new("schema.sql"),
            &case.schema,
            &pgweave_schema::FileRole::Schema,
            parsed.as_ref(),
        );
        b.finish()
    };

    case.queries
        .iter()
        .map(|q| {
            let mut notes = schema_notes.clone();
            // Positional SQL, like the one PostgreSQL prepares: same
            // parameter numbering.
            let outcome = match pgweave_parser::parse_with_outcome(&q.positional_sql()) {
                Ok(o) => o,
                Err(e) => {
                    notes.push(format!("unreadable query: {e}"));
                    return OurDescription {
                        params: None,
                        columns: None,
                        notes,
                    };
                }
            };
            notes.extend(
                outcome
                    .skipped
                    .iter()
                    .map(|s| format!("query: {}", describe_skip(s))),
            );
            let ts = TypeSystem::new(&schema.catalog);
            match pgweave_query::describe_named(&schema, q) {
                Ok(d) => OurDescription {
                    params: Some(
                        d.params
                            .iter()
                            .map(|p| pg_name(&ts, &ty(&p.ty.pg)))
                            .collect(),
                    ),
                    columns: Some(
                        d.columns
                            .iter()
                            // The protocol describes a domain column by
                            // its base type (`printtup.c`).
                            .map(|c| {
                                (
                                    Some(c.name.clone()),
                                    pg_name(&ts, &ts.base_type(&ty(&c.ty.pg))),
                                )
                            })
                            .collect(),
                    ),
                    notes,
                },
                // Descriptor refusal (type not established, analysis error).
                Err(diags) => {
                    for d in diags {
                        notes.push(format!("analysis: [{}] {}", d.code, d.message));
                    }
                    OurDescription {
                        params: None,
                        columns: None,
                        notes,
                    }
                }
            }
        })
        .collect()
}

/// Reason for a skip, on one line, with the start of the statement concerned.
fn describe_skip(s: &SkippedStatement) -> String {
    let head: String = s
        .sql
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join(" ");
    let reason = match &s.reason {
        SkipReason::Unparsable(why) => format!("unreadable ({why})"),
        SkipReason::Unsupported(what) => format!("not modeled ({what})"),
        SkipReason::UnparsedBody { name, reason } => {
            format!("body of `{name}` not analyzed ({reason})")
        }
    };
    format!("`{head} …` {reason}")
}

/// Type designated by a descriptor's qualified name.
fn ty(qualified: &str) -> Ty {
    pgweave_query::descriptor::parse_qualified(qualified)
        .unwrap_or_else(|| pgweave_sema::pg(qualified))
}

/// Type name as `Prepare` reports it: internal name, `[]` for the
/// designated array type of an element.
fn pg_name(ts: &TypeSystem, ty: &Ty) -> String {
    match ts.element(ty) {
        Some(e) if ts.array_of(&e).as_ref() == Some(ty) => format!("{}[]", e.name),
        _ => ty.name.clone(),
    }
}
