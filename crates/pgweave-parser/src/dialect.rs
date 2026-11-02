//! `PgweaveDialect` : le dialecte PostgreSQL de sqlparser, complété là où il
//! s'écarte de PostgreSQL 18 (PLAN.md, D1).
//!
//! C'est une **enveloppe**, pas un fork : chaque méthode que
//! `PostgreSqlDialect` redéfinit est déléguée, et [`Dialect::dialect`] renvoie
//! le `TypeId` de `PostgreSqlDialect`, de sorte que les tests internes de
//! sqlparser (`dialect_of!(self is PostgreSqlDialect)`) traitent cette
//! enveloppe exactement comme PostgreSQL. Les méthodes que `PostgreSqlDialect`
//! ne redéfinit pas gardent leur comportement par défaut, identique des deux
//! côtés.
//!
//! Garde-fous : sqlparser est épinglé, et le test `dialect_parity` vérifie
//! que la liste des méthodes déléguées correspond à celles que
//! `PostgreSqlDialect` redéfinit dans la version utilisée. Une montée de
//! version qui en ajoute une fait échouer ce test au lieu de diverger en
//! silence.
//!
//! Seuls deux manques **au niveau des expressions et des options de
//! colonne** sont comblés ici, parce que sqlparser sait les représenter. Les
//! statements que son AST ne peut pas représenter (`DO`, contraintes
//! `EXCLUDE`, …) sont traités par notre propre grammaire (`grammar`).

use std::any::TypeId;

use sqlparser::ast::{ColumnOption, Expr, GeneratedAs, GeneratedExpressionMode, TrimWhereField};
use sqlparser::dialect::{Dialect, PostgreSqlDialect, Precedence};
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;

#[derive(Debug, Default, Clone, Copy)]
pub struct PgweaveDialect;

const PG: PostgreSqlDialect = PostgreSqlDialect {};

/// Précédence de l'opérande d'un moins unaire : au-dessus de `COLLATE`
/// (120 chez sqlparser), au-dessous des crochets (130), de `::` et de `.`.
const UMINUS_PREC: u8 = 125;

/// Méthodes redéfinies par `PostgreSqlDialect` dans sqlparser 0.62, toutes
/// déléguées ci-dessous. Tenue à jour par le test `dialect_parity`.
pub const DELEGATED: &[&str] = &[
    "identifier_quote_style",
    "is_delimited_identifier_start",
    "is_identifier_start",
    "is_identifier_part",
    "supports_unicode_string_literal",
    "is_reserved_for_identifier",
    "is_custom_operator_part",
    "get_next_precedence",
    "supports_filter_during_aggregation",
    "supports_group_by_expr",
    "prec_value",
    "allow_extract_custom",
    "allow_extract_single_quotes",
    "supports_create_index_with_clause",
    "supports_explain_with_utility_options",
    "supports_listen_notify",
    "supports_factorial_operator",
    "supports_bitwise_shift_operators",
    "supports_comment_on",
    "supports_load_extension",
    "supports_named_fn_args_with_colon_operator",
    "supports_named_fn_args_with_expr_name",
    "supports_empty_projections",
    "supports_nested_comments",
    "supports_string_escape_constant",
    "supports_numeric_literal_underscores",
    "supports_array_typedef_with_brackets",
    "supports_geometric_types",
    "supports_set_names",
    "supports_alter_column_type_using",
    "supports_notnull_operator",
    "supports_interval_options",
    "supports_insert_table_alias",
    "supports_create_table_like_parenthesized",
    "supports_select_wildcard_with_alias",
    "supports_comma_separated_trim",
    "supports_xml_expressions",
    "supports_comment_optimizer_hint",
];

impl Dialect for PgweaveDialect {
    fn dialect(&self) -> TypeId {
        TypeId::of::<PostgreSqlDialect>()
    }

    // ── Compléments ──────────────────────────────────────────────────────────

    /// `trim(BOTH FROM x)` : forme PostgreSQL sans caractères à retirer, que
    /// sqlparser refuse (il attend une expression avant `FROM`).
    fn parse_prefix(&self, parser: &mut Parser) -> Option<Result<Expr, ParserError>> {
        // `-x ^ y` : dans `gram.y`, le moins unaire (`UMINUS`) lie plus fort
        // que `^`, `AT TIME ZONE` et `COLLATE`, moins que `[…]`, `::` et `.` ;
        // sqlparser lui donne la précédence de `*`, donc `-(x ^ y)`.
        if let op @ (Token::Minus | Token::Plus) = &parser.peek_token_ref().token {
            let op = if *op == Token::Minus {
                sqlparser::ast::UnaryOperator::Minus
            } else {
                sqlparser::ast::UnaryOperator::Plus
            };
            parser.next_token();
            return Some(
                parser
                    .parse_subexpr(UMINUS_PREC)
                    .map(|operand| Expr::UnaryOp {
                        op,
                        expr: Box::new(operand),
                    }),
            );
        }
        let is_trim = matches!(&parser.peek_token_ref().token, Token::Word(w) if w.keyword == Keyword::TRIM)
            && parser.peek_nth_token_ref(1).token == Token::LParen;
        if !is_trim {
            return None;
        }
        let where_at = |n: usize| match &parser.peek_nth_token_ref(n).token {
            Token::Word(w) => match w.keyword {
                Keyword::BOTH => Some(TrimWhereField::Both),
                Keyword::LEADING => Some(TrimWhereField::Leading),
                Keyword::TRAILING => Some(TrimWhereField::Trailing),
                _ => None,
            },
            _ => None,
        };
        let is_from = |n: usize| matches!(&parser.peek_nth_token_ref(n).token, Token::Word(w) if w.keyword == Keyword::FROM);
        let (trim_where, skip) = match where_at(2) {
            Some(w) if is_from(3) => (Some(w), 4),
            None if is_from(2) => (None, 3),
            _ => return None,
        };
        for _ in 0..skip {
            parser.next_token();
        }
        Some((|| {
            let expr = parser.parse_expr()?;
            parser.expect_token(&Token::RParen)?;
            Ok(Expr::Trim {
                expr: Box::new(expr),
                trim_where,
                trim_what: None,
                trim_characters: None,
            })
        })())
    }

    /// `a IS [NOT] DISTINCT FROM b` : sqlparser lit `b` comme une expression
    /// complète, donc `x IS DISTINCT FROM y AND z` devenait
    /// `x IS DISTINCT FROM (y AND z)`. Dans `gram.y`, l'opérande droit a la
    /// précédence de `IS` : il absorbe `=`, `+`… mais s'arrête à `AND`/`OR`.
    fn parse_infix(
        &self,
        parser: &mut Parser,
        expr: &Expr,
        _precedence: u8,
    ) -> Option<Result<Expr, ParserError>> {
        let kw = |n: usize| match &parser.peek_nth_token_ref(n).token {
            Token::Word(w) => Some(w.keyword),
            _ => None,
        };
        if kw(0) != Some(Keyword::IS) {
            return None;
        }
        let negated = kw(1) == Some(Keyword::NOT);
        let at = if negated { 2 } else { 1 };
        if kw(at) != Some(Keyword::DISTINCT) || kw(at + 1) != Some(Keyword::FROM) {
            return None;
        }
        for _ in 0..at + 2 {
            parser.next_token();
        }
        Some(
            parser
                .parse_subexpr(self.prec_value(Precedence::Is))
                .map(|right| {
                    let (l, r) = (Box::new(expr.clone()), Box::new(right));
                    if negated {
                        Expr::IsNotDistinctFrom(l, r)
                    } else {
                        Expr::IsDistinctFrom(l, r)
                    }
                }),
        )
    }

    /// `GENERATED ALWAYS AS (expr) [STORED | VIRTUAL]`. sqlparser n'accepte
    /// que `STORED` ; PostgreSQL 18 ajoute `VIRTUAL`, qui est aussi le défaut
    /// quand aucun mot-clé ne suit. La forme `… AS IDENTITY` n'est pas
    /// interceptée.
    fn parse_column_option(
        &self,
        parser: &mut Parser,
    ) -> Result<Option<Result<Option<ColumnOption>, ParserError>>, ParserError> {
        let kw = |n: usize| match &parser.peek_nth_token_ref(n).token {
            Token::Word(w) => Some(w.keyword),
            _ => None,
        };
        let is_generated_expr = kw(0) == Some(Keyword::GENERATED)
            && kw(1) == Some(Keyword::ALWAYS)
            && kw(2) == Some(Keyword::AS)
            && parser.peek_nth_token_ref(3).token == Token::LParen;
        if !is_generated_expr {
            return Ok(None);
        }
        for _ in 0..4 {
            parser.next_token();
        }
        Ok(Some((|| {
            let expr = parser.parse_expr()?;
            parser.expect_token(&Token::RParen)?;
            let mode = if parser.parse_keyword(Keyword::STORED) {
                GeneratedExpressionMode::Stored
            } else {
                // `VIRTUAL` explicite ou absent : virtuel (défaut de PG 18).
                let _explicit = parser.parse_keyword(Keyword::VIRTUAL);
                GeneratedExpressionMode::Virtual
            };
            Ok(Some(ColumnOption::Generated {
                generated_as: GeneratedAs::Always,
                sequence_options: None,
                generation_expr: Some(expr),
                generation_expr_mode: Some(mode),
                generated_keyword: true,
            }))
        })()))
    }

    // ── Délégation à PostgreSqlDialect ───────────────────────────────────────

    fn identifier_quote_style(&self, identifier: &str) -> Option<char> {
        PG.identifier_quote_style(identifier)
    }
    fn is_delimited_identifier_start(&self, ch: char) -> bool {
        PG.is_delimited_identifier_start(ch)
    }
    fn is_identifier_start(&self, ch: char) -> bool {
        PG.is_identifier_start(ch)
    }
    fn is_identifier_part(&self, ch: char) -> bool {
        PG.is_identifier_part(ch)
    }
    fn supports_unicode_string_literal(&self) -> bool {
        PG.supports_unicode_string_literal()
    }
    fn is_reserved_for_identifier(&self, kw: Keyword) -> bool {
        PG.is_reserved_for_identifier(kw)
    }
    fn is_custom_operator_part(&self, ch: char) -> bool {
        PG.is_custom_operator_part(ch)
    }
    fn get_next_precedence(&self, parser: &Parser) -> Option<Result<u8, ParserError>> {
        PG.get_next_precedence(parser)
    }
    fn supports_filter_during_aggregation(&self) -> bool {
        PG.supports_filter_during_aggregation()
    }
    fn supports_group_by_expr(&self) -> bool {
        PG.supports_group_by_expr()
    }
    fn prec_value(&self, prec: Precedence) -> u8 {
        PG.prec_value(prec)
    }
    fn allow_extract_custom(&self) -> bool {
        PG.allow_extract_custom()
    }
    fn allow_extract_single_quotes(&self) -> bool {
        PG.allow_extract_single_quotes()
    }
    fn supports_create_index_with_clause(&self) -> bool {
        PG.supports_create_index_with_clause()
    }
    fn supports_explain_with_utility_options(&self) -> bool {
        PG.supports_explain_with_utility_options()
    }
    fn supports_listen_notify(&self) -> bool {
        PG.supports_listen_notify()
    }
    fn supports_factorial_operator(&self) -> bool {
        PG.supports_factorial_operator()
    }
    fn supports_bitwise_shift_operators(&self) -> bool {
        PG.supports_bitwise_shift_operators()
    }
    fn supports_comment_on(&self) -> bool {
        PG.supports_comment_on()
    }
    fn supports_load_extension(&self) -> bool {
        PG.supports_load_extension()
    }
    fn supports_named_fn_args_with_colon_operator(&self) -> bool {
        PG.supports_named_fn_args_with_colon_operator()
    }
    fn supports_named_fn_args_with_expr_name(&self) -> bool {
        PG.supports_named_fn_args_with_expr_name()
    }
    fn supports_empty_projections(&self) -> bool {
        PG.supports_empty_projections()
    }
    fn supports_nested_comments(&self) -> bool {
        PG.supports_nested_comments()
    }
    fn supports_string_escape_constant(&self) -> bool {
        PG.supports_string_escape_constant()
    }
    fn supports_numeric_literal_underscores(&self) -> bool {
        PG.supports_numeric_literal_underscores()
    }
    fn supports_array_typedef_with_brackets(&self) -> bool {
        PG.supports_array_typedef_with_brackets()
    }
    fn supports_geometric_types(&self) -> bool {
        PG.supports_geometric_types()
    }
    fn supports_set_names(&self) -> bool {
        PG.supports_set_names()
    }
    fn supports_alter_column_type_using(&self) -> bool {
        PG.supports_alter_column_type_using()
    }
    fn supports_notnull_operator(&self) -> bool {
        PG.supports_notnull_operator()
    }
    fn supports_interval_options(&self) -> bool {
        PG.supports_interval_options()
    }
    fn supports_insert_table_alias(&self) -> bool {
        PG.supports_insert_table_alias()
    }
    fn supports_create_table_like_parenthesized(&self) -> bool {
        PG.supports_create_table_like_parenthesized()
    }
    fn supports_select_wildcard_with_alias(&self) -> bool {
        PG.supports_select_wildcard_with_alias()
    }
    fn supports_comma_separated_trim(&self) -> bool {
        PG.supports_comma_separated_trim()
    }
    fn supports_xml_expressions(&self) -> bool {
        PG.supports_xml_expressions()
    }
    fn supports_comment_optimizer_hint(&self) -> bool {
        PG.supports_comment_optimizer_hint()
    }
}
