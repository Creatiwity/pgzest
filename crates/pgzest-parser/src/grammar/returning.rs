//! `RETURNING WITH (OLD AS o, NEW AS n)` (PostgreSQL 18), which sqlparser does
//! not read.
//!
//! The clause is removed from the **tokens** before parsing — the other tokens
//! keep their positions — then attached to the converted statement. Like
//! the extraction of `INTO` in a PL/pgSQL body, this is not a
//! text rewrite.

use pgzest_ast::Statement;
use pgzest_ast::stmt::ReturningAliases;
use sqlparser::tokenizer::{Token, TokenWithSpan};

use crate::convert::convert_ident;
use crate::error::ParseError;

fn is_word(t: &Token, w: &str) -> bool {
    matches!(t, Token::Word(word) if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(w))
}

/// Removes `WITH (…)` following a top-level `RETURNING`, and returns the
/// aliases it declares.
pub(crate) fn extract(
    tokens: &mut Vec<TokenWithSpan>,
) -> Result<Option<ReturningAliases>, ParseError> {
    let significant: Vec<usize> = (0..tokens.len())
        .filter(|&i| !matches!(tokens[i].token, Token::Whitespace(_)))
        .collect();
    let tok = |k: usize| significant.get(k).map(|&i| &tokens[i].token);

    let mut depth = 0i32;
    let mut at = None;
    for (k, &i) in significant.iter().enumerate() {
        match &tokens[i].token {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => depth -= 1,
            t if depth == 0
                && is_word(t, "RETURNING")
                && tok(k + 1).is_some_and(|t| is_word(t, "WITH"))
                && tok(k + 2) == Some(&Token::LParen) =>
            {
                at = Some(k + 1);
                break;
            }
            _ => {}
        }
    }
    let Some(with_k) = at else {
        return Ok(None);
    };

    let bad = |what: &str| ParseError::Syntax(format!("RETURNING WITH: {what}"));
    let mut aliases = ReturningAliases::default();
    let mut k = with_k + 2;
    loop {
        let which = match tok(k) {
            Some(t) if is_word(t, "OLD") => true,
            Some(t) if is_word(t, "NEW") => false,
            _ => return Err(bad("OLD or NEW expected")),
        };
        if !tok(k + 1).is_some_and(|t| is_word(t, "AS")) {
            return Err(bad("AS expected"));
        }
        let Some(Token::Word(w)) = tok(k + 2) else {
            return Err(bad("alias expected"));
        };
        let alias = convert_ident(w.clone().into_ident(tokens[significant[k + 2]].span));
        let slot = if which {
            &mut aliases.old
        } else {
            &mut aliases.new
        };
        if slot.replace(alias).is_some() {
            return Err(bad("OLD or NEW named twice"));
        }
        k += 3;
        match tok(k) {
            Some(Token::Comma) => k += 1,
            Some(Token::RParen) => break,
            _ => return Err(bad(", or ) expected")),
        }
    }
    tokens.drain(significant[with_k]..=significant[k]);
    Ok(Some(aliases))
}

/// Attaches the aliases to the data-modifying statement carrying the `RETURNING`.
pub(crate) fn apply(stmt: &mut Statement, aliases: ReturningAliases) -> Result<(), ParseError> {
    let slot = match stmt {
        Statement::Insert(s) if !s.returning.is_empty() => &mut s.returning_aliases,
        Statement::Update(s) if !s.returning.is_empty() => &mut s.returning_aliases,
        Statement::Delete(s) if !s.returning.is_empty() => &mut s.returning_aliases,
        _ => {
            return Err(ParseError::Syntax(
                "RETURNING WITH outside an INSERT, UPDATE or DELETE".into(),
            ));
        }
    };
    *slot = aliases;
    Ok(())
}
