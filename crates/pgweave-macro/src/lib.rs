//! Surface macros: a compact syntax that desugars into standard SQL.
//!
//! Goal: remove the boilerplate of functions and triggers without guessing
//! anything. All information is carried by the syntax.
//!
//! ```text
//! @function user_count(target_org UUID): INTEGER {
//!     DECLARE total INTEGER;
//!     SELECT count(*) INTO total FROM users WHERE org_id = target_org;
//!     RETURN total;
//! }
//! ```
//!
//! # Macros are data
//!
//! No macro is hard-coded: `@function` and `@trigger` are written in the same
//! declarative format as the one offered to the user (see [`def`] and
//! `builtins/`). This is deliberate — an extension mechanism that the bundled
//! macros do not use is an untested mechanism.
//!
//! # Positions
//!
//! Macro bodies are copied verbatim, and [`SourceMap`] keeps the mapping
//! generated offset → source offset. A diagnostic on a function body therefore
//! points to the right line of the `.pgsql`, not to a position in the
//! generated SQL.

pub mod def;
pub mod expand;
pub mod fragment;
pub mod header;
mod lex;

pub use def::{MacroDef, MacroSet};
pub use expand::{Expansion, expand_source, expand_source_at};
pub use fragment::{FragmentDef, fragment_file_diagnostics, parse_fragment_file};
/// Re-export: the position mapping lives in `pgweave-ast`, next to `Span`, so
/// that the parser can use it without depending on this crate.
pub use pgweave_ast::SourceMap;
