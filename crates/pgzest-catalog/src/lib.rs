//! pgzest's PostgreSQL catalog (PLAN.md, D2, D9, D11).
//!
//! - [`model`]: the objects (schemas, types, routines, relations…),
//!   identified by their qualified name, exact case;
//! - [`pack`]: fragments generated from a real PostgreSQL, one per
//!   contributor of the global context (major version, extensions);
//! - [`catalog`]: the in-memory catalog and resolution through
//!   `search_path`.

pub mod apply;
pub mod builtin;
pub mod catalog;
pub mod compat;
pub mod diff;
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
