//! Analyse sémantique de pgweave (PLAN.md D4, P3).
//!
//! Premier étage : le système de types de PostgreSQL, reproduit à partir du
//! catalogue et non d'une table écrite à la main (principe 1) :
//!
//! - [`TypeSystem`] : identité des types, domaines, tableaux, catégories ;
//! - [`coerce`] : chemins de conversion selon le contexte, type commun
//!   (`parse_coerce.c`) ;
//! - [`polymorphic`] : cohérence et résolution des types polymorphes ;
//! - [`resolve`] : choix d'une surcharge de fonction ou d'opérateur
//!   (`parse_func.c`, `parse_oper.c`).
//!
//! Second étage : le typage des expressions ([`expr`]) et des requêtes
//! ([`query`]) dans leur portée ([`scope`]) — colonnes de sortie nommées et
//! typées, paramètres typés par leur premier usage, nullabilité (D5).
//!
//! Chaque règle cite la fonction de PostgreSQL qu'elle reproduit ; un écart
//! est un bug, révélé par l'oracle.

pub mod coerce;
pub mod expr;
pub mod polymorphic;
pub mod query;
pub mod resolve;
pub mod scope;
mod types;
pub mod view;

pub use coerce::{CoercionContext, Pathway};
pub use expr::{Analyzer, Param, Typed};
pub use query::{Description, OutputColumn, figure_colname};
pub use resolve::{Resolution, ResolveError, ResolvedCall};
pub use scope::{Nullability, SemaError};
pub use types::{Ty, TypeSystem, pg};
pub use view::ViewTyper;
