//! Grammaire propre à pgweave : les statements que sqlparser ne lit pas, ou
//! que son AST ne sait pas représenter, écrits pour PostgreSQL 18.
//!
//! Chaque règle pilote le `Parser` de sqlparser et réutilise ses méthodes
//! publiques pour ce qui est commun (noms, types, expressions, requêtes). Le
//! reste des statements passe par `Parser::parse_statement`.

pub(crate) mod ddl;
pub(crate) mod returning;
pub(crate) mod routine;
pub(crate) mod utility;

use pgweave_ast::Statement;

use crate::error::ParseError;
use routine::RoutineBody;

/// Statement lu par la grammaire de pgweave, avec le corps à analyser s'il
/// s'agit d'une routine. `None` : le statement relève de sqlparser.
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

/// Contexte du texte en cours de parse : pour convertir les positions des
/// tokens en offsets octets.
pub(crate) struct Ctx<'a> {
    pub text: &'a str,
    pub index: &'a LineIndex,
}

impl Ctx<'_> {
    pub(crate) fn offset(&self, loc: Location) -> usize {
        self.index.offset(self.text, loc).unwrap_or(0)
    }
}

/// Mot-clé du `n`-ième token significatif à venir.
pub(crate) fn keyword_at(p: &Parser, n: usize) -> Option<Keyword> {
    match &p.peek_nth_token_ref(n).token {
        Token::Word(w) => Some(w.keyword),
        _ => None,
    }
}
