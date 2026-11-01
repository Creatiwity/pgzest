use pgweave_catalog::ProjectCatalog;
use pgweave_parser::{SkipReason, SkippedStatement};
use pgweave_sema::{Analyzer, Ty, TypeSystem};

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
            // Le SQL positionnel, comme celui que prépare PostgreSQL : même
            // numérotation des paramètres.
            let outcome = match pgweave_parser::parse_with_outcome(&q.positional_sql()) {
                Ok(o) => o,
                Err(e) => {
                    notes.push(format!("requête illisible : {e}"));
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
                    .map(|s| format!("requête : {}", describe_skip(s))),
            );
            let [stmt] = outcome.statements.as_slice() else {
                return OurDescription {
                    params: None,
                    columns: None,
                    notes,
                };
            };
            let ts = TypeSystem::new(&schema.catalog);
            let mut analyzer =
                Analyzer::new(TypeSystem::new(&schema.catalog), schema.search_path.clone());
            match analyzer.describe(stmt) {
                Ok(d) => OurDescription {
                    // Un paramètre que rien n'a typé : pgweave ne se prononce
                    // pas sur la liste.
                    params: d
                        .params
                        .iter()
                        .map(|p| p.ty.as_ref().map(|t| pg_name(&ts, t)))
                        .collect(),
                    columns: Some(
                        d.columns
                            .iter()
                            // Le protocole décrit une colonne de domaine par
                            // son type de base (`printtup.c`).
                            .map(|c| (Some(c.name.clone()), pg_name(&ts, &ts.base_type(&c.ty))))
                            .collect(),
                    ),
                    notes,
                },
                Err(e) => {
                    notes.push(format!("analyse : [{}] {}", e.code, e.message));
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

/// Nom du type tel que le rapporte le `Prepare` : nom interne, `[]` pour
/// le tableau attitré d'un élément.
fn pg_name(ts: &TypeSystem, ty: &Ty) -> String {
    match ts.element(ty) {
        Some(e) if ts.array_of(&e).as_ref() == Some(ty) => format!("{}[]", e.name),
        _ => ty.name.clone(),
    }
}
