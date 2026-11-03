//! Requêtes nommées.
//!
//! - **Format de fichier**, figé en P0 : découpage d'un fichier en requêtes,
//!   en-têtes, paramètres `$name` / `$n`, réécriture en paramètres
//!   positionnels. Spécification : `docs/named-queries.md`.
//! - **Descripteur** (PLAN.md §4, P4) : types des paramètres et des
//!   colonnes, nullabilité, cardinalité inférée, dépendances, empreinte.

pub mod descriptor;
mod file;
mod line_index;

pub use descriptor::{
    Descriptor, QueryModule, check_row_reference, describe_named, describe_sql, describe_sql_with,
    resolve_row,
};
pub use file::{
    Cardinality, ExpandedSql, NamedParam, NamedQuery, Params, QueryFile, parse_query_file,
    parse_query_file_with,
};
