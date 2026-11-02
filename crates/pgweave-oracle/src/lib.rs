//! Oracle différentiel (PLAN.md §7).
//!
//! Pour chaque requête d'un corpus de cas, on confronte ce que pgweave
//! affirme (types des paramètres et des colonnes de sortie) à ce que renvoie
//! le `Prepare` d'un vrai PostgreSQL. Tout écart est un bug de pgweave ou une
//! divergence à documenter ; rien n'est inventé pour faire correspondre.
//!
//! Les cas vivent dans `cases/<nom>/` : un `schema.sql` appliqué sur une base
//! vierge, et un `queries.sql` au format des requêtes nommées
//! (`docs/named-queries.md`).
//!
//! Le rapport produit est un instantané (`insta`) : il sert de référence.
//! Une amélioration de l'analyse le fait changer, et la revue de ce
//! changement montre exactement ce qui a progressé — ou régressé.
//!
//! Le module [`schema`] confronte de même les migrations générées (P7) :
//! migrations appliquées et schéma déclaratif doivent donner la même base.
//!
//! Limite actuelle : seuls les **types** sont confrontés. PostgreSQL ne
//! renvoie pas la nullabilité d'une colonne de sortie ; un oracle partiel
//! (colonnes issues directement d'une table, via `pg_attribute.attnotnull`)
//! sera ajouté avec l'inférence de nullabilité (P3).

mod case;
mod ours;
mod pg;
mod report;
pub mod schema;

pub use case::{Case, load_cases};
pub use ours::{OurDescription, describe_ours};
pub use pg::{PgDescription, URL_ENV, describe_case, server_major_version};
pub use report::{CaseReport, Totals, render};
