//! Catalogue PostgreSQL de pgweave (PLAN.md, D2, D9, D11).
//!
//! - [`model`] : les objets (schémas, types, routines, relations…), identifiés
//!   par leur nom qualifié, casse exacte ;
//! - [`pack`] : fragments générés depuis un PostgreSQL réel, un par
//!   contributeur du contexte global (version majeure, extensions) ;
//! - [`catalog`] : le catalogue en mémoire et la résolution par
//!   `search_path`.

pub mod apply;
pub mod builtin;
pub mod catalog;
pub mod compat;
pub mod dump;
pub mod model;
pub mod names;
pub mod pack;
pub mod project;
pub mod query;

pub use apply::{QueryTyper, Replay};
pub use catalog::{Catalog, SearchPath};
pub use model::{QName, TypeRef};
pub use pack::{Contributor, Pack, PackError};
pub use project::{ProjectCatalog, StatementChecker};
