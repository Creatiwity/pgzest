//! Tokenization of a PL/pgSQL body by pgweave's dialect, with one
//! correction: the `..` range operator of `FOR` loops.
//!
//! The SQL tokenizer reads `1..10` as `1.` then `.10` (two decimal
//! numbers). The PL/pgSQL lexer recognizes `..` first: we restore
//! that reading on the tokens, positions included, without touching the text.

use sqlparser::tokenizer::{Location, Span, Token, TokenWithSpan, Tokenizer};

use crate::dialect::PgweaveDialect;

/// Tokens of the body, or (position, message) of the tokenization error.
pub(crate) fn tokenize(body: &str) -> Result<Vec<TokenWithSpan>, (Location, String)> {
    let raw = Tokenizer::new(&PgweaveDialect, body)
        .tokenize_with_location()
        .map_err(|e| (e.location, e.message))?;
    Ok(split_dot_dot(raw))
}

fn split_dot_dot(tokens: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(tokens.len());
    for tok in tokens {
        // `.5` glued to what precedes, when what precedes ends with a dot:
        // it is `..` followed by `5`.
        if let Token::Number(n, long) = &tok.token
            && let Some(rest) = n.strip_prefix('.')
            && let Some(prev) = out.last_mut()
            && prev.span.end == tok.span.start
        {
            let dot_before = match &mut prev.token {
                // `1.` → `1` then `.`
                Token::Number(p, _) if p.ends_with('.') && p.len() > 1 => {
                    p.pop();
                    let dot = shift(prev.span.end, -1);
                    prev.span.end = dot;
                    Some(dot)
                }
                // `n.` is already `n` then `.`
                Token::Period => None,
                _ => {
                    out.push(tok);
                    continue;
                }
            };
            if let Some(dot) = dot_before {
                out.push(TokenWithSpan::new(
                    Token::Period,
                    Span::new(dot, shift(dot, 1)),
                ));
            }
            let second = tok.span.start;
            out.push(TokenWithSpan::new(
                Token::Period,
                Span::new(second, shift(second, 1)),
            ));
            out.push(TokenWithSpan::new(
                Token::Number(rest.to_string(), *long),
                Span::new(shift(second, 1), tok.span.end),
            ));
            continue;
        }
        out.push(tok);
    }
    out
}

/// Shifts a position on the same line (numbers are ASCII, without line
/// breaks).
fn shift(loc: Location, by: i64) -> Location {
    Location {
        line: loc.line,
        column: (loc.column as i64 + by) as u64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<String> {
        tokenize(s)
            .unwrap()
            .into_iter()
            .filter(|t| !matches!(t.token, Token::Whitespace(_)))
            .map(|t| t.token.to_string())
            .collect()
    }

    #[test]
    fn range_of_numbers() {
        assert_eq!(kinds("1..10"), ["1", ".", ".", "10"]);
    }

    #[test]
    fn range_from_a_variable() {
        assert_eq!(kinds("n..1"), ["n", ".", ".", "1"]);
    }

    #[test]
    fn a_decimal_stays_a_decimal() {
        assert_eq!(kinds("1.5 + .5"), ["1.5", "+", ".5"]);
    }

    #[test]
    fn positions_follow() {
        let toks: Vec<_> = tokenize("1..10")
            .unwrap()
            .into_iter()
            .map(|t| (t.span.start.column, t.span.end.column))
            .collect();
        assert_eq!(toks, [(1, 2), (2, 3), (3, 4), (4, 6)]);
    }
}
