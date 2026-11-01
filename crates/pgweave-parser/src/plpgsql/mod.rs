//! Parser PL/pgSQL.
//!
//! Le corps est tokenisé par le dialecte de pgweave et lu sur ces tokens, à
//! leurs positions réelles : les statements SQL qu'il contient passent par le
//! même chemin que ceux d'un fichier, sans reconstruction de texte.

pub mod error;
mod parser;
mod tokens;

pub use error::PlParseError;
pub use parser::{parse_plpgsql_body, parse_plpgsql_body_at, parse_plpgsql_body_mapped};
