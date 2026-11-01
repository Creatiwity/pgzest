use pgweave_ast::Statement;
use pgweave_ast::types::DataType;
use pgweave_catalog::ProjectCatalog;
use pgweave_parser::{SkipReason, SkippedStatement};
use pgweave_query::Params;

use crate::case::Case;

/// Ce que pgweave affirme d'une requête. `None` = pgweave ne se prononce pas
/// (« non inféré »), ce qui est distinct d'une liste vide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OurDescription {
    pub params: Option<Vec<String>>,
    /// Nom (si pgweave en déduit un) et type de chaque colonne de sortie.
    pub columns: Option<Vec<(Option<String>, String)>>,
    /// Ce qui a dégradé l'analyse : statements du schéma ou de la requête
    /// écartés par le parser. Vide = analyse complète.
    pub notes: Vec<String>,
}

/// Décrit chaque requête du cas avec l'analyse actuelle de pgweave, telle
/// que la consommerait un générateur de code (`OutputColumn`).
pub fn describe_ours(case: &Case) -> Vec<OurDescription> {
    // Le résolveur ne dit pas ce qu'il a écarté : on reparse pour le savoir.
    // Une table absente de l'index explique des colonnes « fausses » en aval,
    // le rapport doit donc le montrer.
    let schema_notes: Vec<String> = match pgweave_parser::parse_with_outcome(&case.schema) {
        Ok(o) => o
            .skipped
            .iter()
            .map(|s| format!("schéma : {}", describe_skip(s)))
            .collect(),
        Err(e) => vec![format!("schéma illisible : {e}")],
    };
    let schema = ProjectCatalog::from_sql(&case.schema).unwrap_or_else(|_| ProjectCatalog::empty());

    case.queries
        .iter()
        .map(|q| {
            let mut notes = schema_notes.clone();
            let params = match q.params {
                // Aucune inférence de type de paramètre n'existe encore :
                // pgweave ne peut se prononcer que sur l'absence de paramètre.
                Params::None => Some(Vec::new()),
                _ => None,
            };
            let outcome = match pgweave_parser::parse_with_outcome(&q.sql) {
                Ok(o) => o,
                Err(e) => {
                    notes.push(format!("requête illisible : {e}"));
                    return OurDescription {
                        params,
                        columns: None,
                        notes,
                    };
                }
            };
            notes.extend(
                outcome
                    .skipped
                    .iter()
                    .map(|s| format!("requête : {}", describe_skip(s))),
            );
            let [stmt] = outcome.statements.as_slice() else {
                return OurDescription {
                    params,
                    columns: None,
                    notes,
                };
            };
            let semantic = pgweave_semantic::analyze(&outcome.statements, &schema);
            let columns = match stmt {
                Statement::Select(s) => semantic.select_columns.get(&s.id).map(|cols| {
                    cols.iter()
                        .map(|c| (c.name.clone(), pg_name(&c.data_type)))
                        .collect()
                }),
                Statement::Insert(s) if s.returning.is_empty() => Some(Vec::new()),
                Statement::Update(s) if s.returning.is_empty() => Some(Vec::new()),
                Statement::Delete(s) if s.returning.is_empty() => Some(Vec::new()),
                _ => None,
            };
            OurDescription {
                params,
                columns,
                notes,
            }
        })
        .collect()
}

/// Raison d'un écart, sur une ligne, avec le début du statement concerné.
fn describe_skip(s: &SkippedStatement) -> String {
    let head: String = s
        .sql
        .split_whitespace()
        .take(4)
        .collect::<Vec<_>>()
        .join(" ");
    let reason = match &s.reason {
        SkipReason::Unparsable(why) => format!("illisible ({why})"),
        SkipReason::Unsupported(what) => format!("non modélisé ({what})"),
        SkipReason::UnparsedBody { name, reason } => {
            format!("corps de `{name}` non analysé ({reason})")
        }
    };
    format!("`{head} …` {reason}")
}

/// Nom interne PostgreSQL du type, pour comparaison avec le `Prepare`.
fn pg_name(dt: &DataType) -> String {
    use DataType::*;
    match dt {
        SmallInt => "int2".into(),
        Integer => "int4".into(),
        BigInt => "int8".into(),
        Numeric { .. } => "numeric".into(),
        Real => "float4".into(),
        DoublePrecision => "float8".into(),
        Text => "text".into(),
        Varchar(_) => "varchar".into(),
        Char(_) => "bpchar".into(),
        Date => "date".into(),
        Time(_) => "time".into(),
        TimeTz(_) => "timetz".into(),
        Timestamp(_) => "timestamp".into(),
        TimestampTz(_) => "timestamptz".into(),
        Interval => "interval".into(),
        Boolean => "bool".into(),
        Bytea => "bytea".into(),
        Json => "json".into(),
        Jsonb => "jsonb".into(),
        Uuid => "uuid".into(),
        Array(inner) => format!("{}[]", pg_name(inner)),
        Custom(name) => name.clone(),
        // Le `Prepare` rapporte le type sans ses modificateurs.
        Modified { name, .. } => name.clone(),
    }
}
