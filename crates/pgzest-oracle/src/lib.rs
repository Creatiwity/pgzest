//! Differential oracle (PLAN.md §7).
//!
//! For each query of a case corpus, what pgzest claims (types of the
//! parameters and of the output columns) is checked against what the
//! `Prepare` of a real PostgreSQL returns. Any gap is a pgzest bug or a
//! divergence to document; nothing is made up to make them match.
//!
//! Cases live in `cases/<name>/`: a `schema.sql` applied to an empty
//! database, and a `queries.sql` in the named-query format
//! (`site/src/content/docs/reference/named-queries.md`).
//!
//! The produced report is a snapshot (`insta`): it serves as the reference.
//! An analysis improvement changes it, and reviewing that change shows
//! exactly what progressed, or regressed.
//!
//! The [`schema`] module likewise checks the generated migrations (P7):
//! applied migrations and the declarative schema must yield the same database.
//!
//! Current limit: only **types** are compared. PostgreSQL does not return
//! the nullability of an output column; a partial oracle (columns taken
//! directly from a table, via `pg_attribute.attnotnull`) will be added with
//! nullability inference (P3).

mod case;
mod ours;
pub mod pack_gen;
mod pg;
mod report;
pub mod schema;

pub use case::{Case, load_cases};
pub use ours::{OurDescription, describe_ours};
pub use pg::{PgDescription, URL_ENV, describe_case, server_major_version};
pub use report::{CaseReport, Totals, render};
