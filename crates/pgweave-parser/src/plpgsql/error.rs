use thiserror::Error;

#[derive(Debug, Error, Clone)]
pub enum PlParseError {
    #[error("syntax error at position {pos}: {message}")]
    Syntax { pos: usize, message: String },
    #[error("unexpected token '{token}' at position {pos}")]
    UnexpectedToken { token: String, pos: usize },
    #[error("unexpected end of input")]
    UnexpectedEof,
    #[error("unsupported construct: {0}")]
    Unsupported(String),
}
