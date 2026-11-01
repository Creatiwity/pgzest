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
//! Chaque règle cite la fonction de PostgreSQL qu'elle reproduit ; un écart
//! est un bug, révélé par l'oracle.

pub mod coerce;
pub mod polymorphic;
pub mod resolve;
mod types;

pub use coerce::{CoercionContext, Pathway};
pub use resolve::{Resolution, ResolveError, ResolvedCall};
pub use types::{Ty, TypeSystem, pg};
