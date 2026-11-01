//! Statements utilitaires que sqlparser ne lit pas dans leur forme
//! PostgreSQL : `ANALYZE` et `VACUUM` (sqlparser ne connaît que la forme Hive,
//! à une seule table).

use pgweave_ast::Statement;
use pgweave_ast::node::Span;
use pgweave_ast::utility::{
    MaintenanceCommand, MaintenanceOption, MaintenanceTarget, Utility, UtilityKind,
};
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;

use super::keyword_at;
use crate::convert::{convert_ident, convert_object_name};
use crate::error::ParseError;

type Result<T> = std::result::Result<T, ParseError>;

pub(crate) fn is_maintenance(p: &Parser) -> bool {
    matches!(keyword_at(p, 0), Some(Keyword::ANALYZE | Keyword::VACUUM))
        || matches!(&p.peek_token_ref().token, Token::Word(w) if w.value.eq_ignore_ascii_case("analyse"))
}

/// `ANALYZE [(option [valeur], …) | VERBOSE] [table [(colonnes)], …]`
/// `VACUUM [(option [valeur], …) | FULL FREEZE VERBOSE ANALYZE] [table [(colonnes)], …]`
pub(crate) fn parse_maintenance(p: &mut Parser) -> Result<Statement> {
    let command = match &p.next_token().token {
        Token::Word(w) if w.keyword == Keyword::VACUUM => MaintenanceCommand::Vacuum,
        _ => MaintenanceCommand::Analyze,
    };
    let mut options = Vec::new();
    if p.consume_token(&Token::LParen) {
        loop {
            let name = word(p)?;
            let value = match &p.peek_token_ref().token {
                Token::Comma | Token::RParen => None,
                Token::Number(n, _) => {
                    let n = n.clone();
                    p.next_token();
                    Some(n)
                }
                Token::SingleQuotedString(v) => {
                    let v = v.clone();
                    p.next_token();
                    Some(v)
                }
                _ => Some(word(p)?),
            };
            options.push(MaintenanceOption {
                name,
                value,
                legacy: false,
            });
            if p.consume_token(&Token::RParen) {
                break;
            }
            p.expect_token(&Token::Comma).map_err(pe)?;
        }
    } else {
        let legacy: &[Keyword] = match command {
            MaintenanceCommand::Analyze => &[Keyword::VERBOSE],
            MaintenanceCommand::Vacuum => &[
                Keyword::FULL,
                Keyword::FREEZE,
                Keyword::VERBOSE,
                Keyword::ANALYZE,
            ],
        };
        while let Some(k) = p.parse_one_of_keywords(legacy) {
            options.push(MaintenanceOption {
                name: format!("{k:?}"),
                value: None,
                legacy: true,
            });
        }
    }
    let mut tables = Vec::new();
    if p.peek_token_ref().token != Token::EOF {
        loop {
            let table = convert_object_name(p.parse_object_name(false).map_err(pe)?);
            let columns = if p.consume_token(&Token::LParen) {
                let cols = p
                    .parse_comma_separated(|p| p.parse_identifier())
                    .map_err(pe)?;
                p.expect_token(&Token::RParen).map_err(pe)?;
                cols.into_iter().map(convert_ident).collect()
            } else {
                Vec::new()
            };
            tables.push(MaintenanceTarget { table, columns });
            if !p.consume_token(&Token::Comma) {
                break;
            }
        }
    }
    if p.peek_token_ref().token != Token::EOF {
        let found = p.peek_token_ref().clone();
        return Err(pe(p
            .expected_ref::<()>("fin du statement", &found)
            .unwrap_err()));
    }
    Ok(Statement::Utility(Utility {
        span: Span::DUMMY,
        kind: UtilityKind::Maintenance {
            command,
            options,
            tables,
        },
    }))
}

/// Mot (mot-clé ou identifiant), en majuscules.
fn word(p: &mut Parser) -> Result<String> {
    match &p.next_token().token {
        Token::Word(w) => Ok(w.value.to_ascii_uppercase()),
        _ => {
            p.prev_token();
            let found = p.peek_token_ref().clone();
            Err(pe(p.expected_ref::<()>("une option", &found).unwrap_err()))
        }
    }
}

fn pe(e: ParserError) -> ParseError {
    ParseError::Syntax(e.to_string())
}
