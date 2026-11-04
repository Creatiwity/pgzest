//! Semantic analysis for pgzest (PLAN.md D4, P3).
//!
//! First stage: the PostgreSQL type system, reproduced from the catalog and
//! not from a hand-written table (principle 1):
//!
//! - [`TypeSystem`]: type identity, domains, arrays, categories;
//! - [`coerce`]: conversion pathways by context, common type
//!   (`parse_coerce.c`);
//! - [`polymorphic`]: consistency and resolution of polymorphic types;
//! - [`resolve`]: choice of a function or operator overload
//!   (`parse_func.c`, `parse_oper.c`).
//!
//! Second stage: typing of expressions ([`expr`]) and queries
//! ([`query`]) in their scope ([`scope`]) — named and typed output
//! columns, parameters typed by their first use, nullability (D5).
//!
//! Each rule cites the PostgreSQL function it reproduces; a discrepancy
//! is a bug, revealed by the oracle.

pub mod check;
pub mod coerce;
pub mod expr;
pub mod polymorphic;
pub mod query;
pub mod resolve;
pub mod routine;
pub mod scope;
mod types;
pub mod view;

pub use coerce::{CoercionContext, Pathway};
pub use expr::{Analyzer, Param, Typed};
pub use query::{Description, OutputColumn, figure_colname};
pub use resolve::{Resolution, ResolveError, ResolvedCall};
pub use scope::{Nullability, SemaError};
pub use types::{Dependency, Ty, TypeSystem, pg};
pub use view::ViewTyper;
