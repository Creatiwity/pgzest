//! Named queries.
//!
//! - **File format**, frozen in P0: splitting a file into queries, headers,
//!   `$name` / `$n` parameters, rewriting into positional parameters.
//!   Specification: `docs/named-queries.md`.
//! - **Descriptor** (PLAN.md §4, P4): parameter and column types,
//!   nullability, inferred cardinality, dependencies, fingerprint.

pub mod descriptor;
pub mod dynamic;
pub(crate) mod file;
mod line_index;

pub use descriptor::{
    Descriptor, QueryModule, QuerySource, check_row_reference, describe_named, describe_sql,
    describe_sql_with, resolve_row,
};
pub use file::{
    Cardinality, Dynamic, ExpandedSql, NamedParam, NamedQuery, Params, QueryFile,
    blank_optional_markers, parse_query_file, parse_query_file_with,
};
