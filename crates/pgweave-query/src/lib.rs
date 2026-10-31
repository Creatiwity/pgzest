//! Requêtes nommées.
//!
//! Pour l'instant, ce crate porte le **format de fichier** des requêtes
//! nommées, figé en P0 : découpage d'un fichier en requêtes, en-têtes,
//! paramètres `$name` / `$n`, réécriture en paramètres positionnels.
//! Spécification de référence : `docs/named-queries.md`.
//!
//! Le descripteur de requête (types, nullabilité, cardinalité inférée —
//! PLAN.md §4) viendra s'y ajouter en P4.

mod file;
mod line_index;

pub use file::{Cardinality, NamedParam, NamedQuery, Params, QueryFile, parse_query_file};
