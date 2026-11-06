//! `PgzestDialect`: sqlparser's PostgreSQL dialect, completed where it
//! diverges from PostgreSQL 18 (PLAN.md, D1).
//!
//! It is a **wrapper**, not a fork: every method that
//! `PostgreSqlDialect` overrides is delegated, and [`Dialect::dialect`] returns
//! the `TypeId` of `PostgreSqlDialect`, so that sqlparser's internal checks
//! (`dialect_of!(self is PostgreSqlDialect)`) treat this
//! wrapper exactly like PostgreSQL. Methods that `PostgreSqlDialect`
//! does not override keep their default behavior, identical on both
//! sides.
//!
//! Safeguards: sqlparser is pinned, and the `dialect_parity` test checks
//! that the list of delegated methods matches those that
//! `PostgreSqlDialect` overrides in the version in use. A version bump
//! that adds one makes this test fail instead of silently
//! diverging.
//!
//! Only two gaps **at the level of expressions and column
//! options** are filled here, because sqlparser can represent them.
//! Statements its AST cannot represent (`DO`, `EXCLUDE`
//! constraints, …) are handled by our own grammar (`grammar`).

use std::any::TypeId;

use sqlparser::ast::{ColumnOption, Expr, GeneratedAs, GeneratedExpressionMode, TrimWhereField};
use sqlparser::dialect::{Dialect, PostgreSqlDialect, Precedence};
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;

#[derive(Debug, Default, Clone, Copy)]
pub struct PgzestDialect;

const PG: PostgreSqlDialect = PostgreSqlDialect {};

/// Precedence of the operand of a unary minus: above `COLLATE`
/// (120 in sqlparser), below brackets (130), `::` and `.`.
const UMINUS_PREC: u8 = 125;

/// Methods overridden by `PostgreSqlDialect` in sqlparser 0.63, all
/// delegated below. Kept up to date by the `dialect_parity` test.
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
    "is_table_alias",
    "supports_aliased_function_args",
    "supports_alter_user_as_alter_role",
    "supports_exclude_constraint",
    "supports_left_associative_joins_without_parens",
    "supports_order_by_using_operator",
];

impl Dialect for PgzestDialect {
    fn dialect(&self) -> TypeId {
        TypeId::of::<PostgreSqlDialect>()
    }

    // ── Additions ────────────────────────────────────────────────────────────

    /// `trim(BOTH FROM x)`: PostgreSQL form without characters to remove, which
    /// sqlparser rejects (it expects an expression before `FROM`).
    fn parse_prefix(&self, parser: &mut Parser) -> Option<Result<Expr, ParserError>> {
        // `-x ^ y`: in `gram.y`, unary minus (`UMINUS`) binds tighter
        // than `^`, `AT TIME ZONE` and `COLLATE`, looser than `[…]`, `::` and `.`;
        // sqlparser gives it the precedence of `*`, hence `-(x ^ y)`.
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

    /// `a IS [NOT] DISTINCT FROM b`: sqlparser reads `b` as a full
    /// expression, so `x IS DISTINCT FROM y AND z` became
    /// `x IS DISTINCT FROM (y AND z)`. In `gram.y`, the right operand has the
    /// precedence of `IS`: it absorbs `=`, `+`… but stops at `AND`/`OR`.
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

    /// `GENERATED ALWAYS AS (expr) [STORED | VIRTUAL]`. sqlparser only accepts
    /// `STORED`; PostgreSQL 18 adds `VIRTUAL`, which is also the default
    /// when no keyword follows. The `… AS IDENTITY` form is not
    /// intercepted.
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
                // Explicit or absent `VIRTUAL`: virtual (PG 18 default).
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

    // ── Delegation to PostgreSqlDialect ──────────────────────────────────────

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
    fn is_table_alias(&self, kw: &Keyword, parser: &mut Parser) -> bool {
        PG.is_table_alias(kw, parser)
    }
    fn supports_aliased_function_args(&self) -> bool {
        PG.supports_aliased_function_args()
    }
    fn supports_alter_user_as_alter_role(&self) -> bool {
        PG.supports_alter_user_as_alter_role()
    }
    fn supports_exclude_constraint(&self) -> bool {
        PG.supports_exclude_constraint()
    }
    fn supports_left_associative_joins_without_parens(&self) -> bool {
        PG.supports_left_associative_joins_without_parens()
    }
    fn supports_order_by_using_operator(&self) -> bool {
        PG.supports_order_by_using_operator()
    }
}
