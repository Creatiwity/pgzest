//! Tokenisation d'un corps PL/pgSQL par le dialecte de pgweave, avec une
//! correction : l'opérateur d'intervalle `..` des boucles `FOR`.
//!
//! Le tokenizer SQL lit `1..10` comme `1.` puis `.10` (deux nombres
//! décimaux). Le lexer de PL/pgSQL reconnaît `..` en priorité : on rétablit
//! cette lecture sur les tokens, positions comprises, sans toucher au texte.

use sqlparser::tokenizer::{Location, Span, Token, TokenWithSpan, Tokenizer};

use crate::dialect::PgweaveDialect;

/// Tokens du corps, ou (position, message) de l'erreur de tokenisation.
pub(crate) fn tokenize(body: &str) -> Result<Vec<TokenWithSpan>, (Location, String)> {
    let raw = Tokenizer::new(&PgweaveDialect, body)
        .tokenize_with_location()
        .map_err(|e| (e.location, e.message))?;
    Ok(split_dot_dot(raw))
}

fn split_dot_dot(tokens: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(tokens.len());
    for tok in tokens {
        // `.5` collé à ce qui précède, quand ce qui précède finit par un point :
        // c'est `..` suivi de `5`.
        if let Token::Number(n, long) = &tok.token
            && let Some(rest) = n.strip_prefix('.')
            && let Some(prev) = out.last_mut()
            && prev.span.end == tok.span.start
        {
            let dot_before = match &mut prev.token {
                // `1.` → `1` puis `.`
                Token::Number(p, _) if p.ends_with('.') && p.len() > 1 => {
                    p.pop();
                    let dot = shift(prev.span.end, -1);
                    prev.span.end = dot;
                    Some(dot)
                }
                // `n.` est déjà `n` puis `.`
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

/// Décale une position sur la même ligne (les nombres sont ASCII, sans saut
/// de ligne).
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
    fn intervalle_de_nombres() {
        assert_eq!(kinds("1..10"), ["1", ".", ".", "10"]);
    }

    #[test]
    fn intervalle_depuis_une_variable() {
        assert_eq!(kinds("n..1"), ["n", ".", ".", "1"]);
    }

    #[test]
    fn un_decimal_reste_un_decimal() {
        assert_eq!(kinds("1.5 + .5"), ["1.5", "+", ".5"]);
    }

    #[test]
    fn les_positions_suivent() {
        let toks: Vec<_> = tokenize("1..10")
            .unwrap()
            .into_iter()
            .map(|t| (t.span.start.column, t.span.end.column))
            .collect();
        assert_eq!(toks, [(1, 2), (2, 3), (3, 4), (4, 6)]);
    }
}
