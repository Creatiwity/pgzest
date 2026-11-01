//! Grammaire propre à pgweave : les statements que sqlparser ne lit pas, ou
//! que son AST ne sait pas représenter, écrits pour PostgreSQL 18.
//!
//! Chaque règle pilote le `Parser` de sqlparser et réutilise ses méthodes
//! publiques pour ce qui est commun (noms, types, expressions, requêtes). Le
//! reste des statements passe par `Parser::parse_statement`.

pub(crate) mod routine;

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
