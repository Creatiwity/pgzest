mod convert;
pub mod error;

pub use error::ParseError;

use pgsql_ast::Statement;
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser as SqlParser;

pub fn parse(sql: &str) -> Result<Vec<Statement>, ParseError> {
    let dialect = PostgreSqlDialect {};
    let stmts = SqlParser::parse_sql(&dialect, sql)
        .map_err(|e| ParseError::Syntax(e.to_string()))?;
    stmts.into_iter().map(convert::convert_statement).collect()
}

/// Expose single-statement conversion for testing purposes.
/// Takes a pre-parsed sqlparser statement and converts it to a pgsql_ast Statement.
pub fn convert_statement_pub(
    stmt: sqlparser::ast::Statement,
) -> Result<Statement, ParseError> {
    convert::convert_statement(stmt)
}
