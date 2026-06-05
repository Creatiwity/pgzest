use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("syntax error: {0}")]
    Syntax(String),
    #[error("unsupported statement: {0}")]
    Unsupported(String),
}
