//! pgweave's own grammar: statements sqlparser does not read, or
//! that its AST cannot represent, written for PostgreSQL 18.
//!
//! Each rule drives sqlparser's `Parser` and reuses its public methods
//! for what is common (names, types, expressions, queries). The
//! remaining statements go through `Parser::parse_statement`.

pub(crate) mod ddl;
pub(crate) mod returning;
pub(crate) mod routine;
pub(crate) mod utility;

use pgweave_ast::Statement;

use crate::error::ParseError;
use routine::RoutineBody;

/// Statement read by pgweave's grammar, with the body to analyze if it
/// is a routine. `None`: the statement belongs to sqlparser.
pub(crate) fn parse(
    p: &mut Parser,
    ctx: &Ctx,
) -> Option<Result<(Statement, Option<RoutineBody>), ParseError>> {
    let plain = |r: Result<Statement, ParseError>| Some(r.map(|s| (s, None)));
    if routine::is_routine(p) {
        return Some(routine::parse_routine(p, ctx).map(|r| (r.statement, r.body)));
    }
    if routine::is_do(p) {
        return Some(routine::parse_do(p, ctx).map(|r| (r.statement, r.body)));
    }
    if ddl::is_create_table(p) {
        return plain(ddl::parse_create_table(p));
    }
    if ddl::is_alter_table(p) {
        return plain(ddl::parse_alter_table(p));
    }
    if ddl::is_create_domain(p) {
        return plain(ddl::parse_create_domain(p));
    }
    if ddl::is_create_extension(p) {
        return plain(ddl::parse_create_extension(p));
    }
    if ddl::is_alter_extension(p) {
        return plain(ddl::parse_alter_extension(p));
    }
    if ddl::is_create_sequence(p) {
        return plain(ddl::parse_create_sequence(p));
    }
    if ddl::is_alter_type_set_schema(p) {
        return plain(ddl::parse_alter_type_set_schema(p));
    }
    if utility::is_maintenance(p) {
        return plain(utility::parse_maintenance(p));
    }
    None
}

use sqlparser::keywords::Keyword;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Location, Token};

use crate::split::LineIndex;

/// Context of the text being parsed: to convert token positions
/// into byte offsets.
pub(crate) struct Ctx<'a> {
    pub text: &'a str,
    pub index: &'a LineIndex,
}

impl Ctx<'_> {
    pub(crate) fn offset(&self, loc: Location) -> usize {
        self.index.offset(self.text, loc).unwrap_or(0)
    }
}

/// Keyword of the `n`-th upcoming significant token.
pub(crate) fn keyword_at(p: &Parser, n: usize) -> Option<Keyword> {
    match &p.peek_nth_token_ref(n).token {
        Token::Word(w) => Some(w.keyword),
        _ => None,
    }
}
