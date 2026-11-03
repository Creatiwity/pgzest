//! PL/pgSQL parser.
//!
//! The body is tokenized by pgweave's dialect and read from those tokens, at
//! their real positions: the SQL statements it contains go through the
//! same path as those of a file, without rebuilding any text.

pub mod error;
mod parser;
mod tokens;

pub use error::PlParseError;
pub use parser::{parse_plpgsql_body, parse_plpgsql_body_at, parse_plpgsql_body_mapped};
