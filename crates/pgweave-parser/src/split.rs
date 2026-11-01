//! Découpage d'un texte en statements, sur les tokens.
//!
//! Le texte entier est tokenisé une fois par le dialecte de pgweave : les
//! chaînes, chaînes dollar, identifiants entre guillemets et commentaires sont
//! des tokens atomiques, donc un `;` qu'ils contiennent ne coupe jamais un
//! statement. Les positions des tokens sont celles du texte : le parse de
//! chaque statement les réutilise telles quelles, sans recopie ni décalage.
//!
//! Remplace un découpage textuel octet par octet, qui devait réimplémenter à
//! la main la lexicographie de PostgreSQL (et la ratait sur les cas rares).

use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer};

use crate::dialect::PgweaveDialect;

/// Un morceau du texte : un statement à parser, ou une zone que le tokenizer
/// n'a pas su lire.
pub(crate) enum Unit {
    Statement(StatementTokens),
    /// Texte illisible jusqu'à la fin (chaîne non terminée…). Les statements
    /// complets qui précèdent ont été découpés normalement.
    Unreadable {
        start: usize,
        end: usize,
        message: String,
    },
}

pub(crate) struct StatementTokens {
    /// Tokens du statement, sans le `;` final ni les blancs qui le précèdent.
    pub tokens: Vec<TokenWithSpan>,
    /// Offset octet du premier token significatif, dans le texte.
    pub start: usize,
    /// Offset octet de fin du dernier token significatif.
    pub end: usize,
}

/// Index des débuts de ligne, pour traduire une `Location` en offset octet.
pub(crate) struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub(crate) fn new(text: &str) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(
            text.bytes()
                .enumerate()
                .filter(|(_, b)| *b == b'\n')
                .map(|(i, _)| i + 1),
        );
        Self { line_starts }
    }

    /// Offset octet d'une position sqlparser (ligne et colonne 1-basées,
    /// colonne en caractères).
    pub(crate) fn offset(&self, text: &str, loc: Location) -> Option<usize> {
        if loc.line == 0 || loc.column == 0 {
            return None;
        }
        let line_start = *self.line_starts.get((loc.line - 1) as usize)?;
        let line_end = self
            .line_starts
            .get(loc.line as usize)
            .copied()
            .unwrap_or(text.len());
        let line = text.get(line_start..line_end)?;
        let in_line = line
            .char_indices()
            .nth((loc.column - 1) as usize)
            .map(|(b, _)| b)
            .unwrap_or(line.len());
        Some(line_start + in_line)
    }
}

/// Découpe `text` en statements.
pub(crate) fn split(text: &str) -> Vec<Unit> {
    let index = LineIndex::new(text);
    let mut units = Vec::new();

    let (tokens, failure) = match tokenize(text) {
        Ok(tokens) => (tokens, None),
        Err((at, message)) => {
            // On garde ce qui précède l'erreur : les statements complets avant
            // elle sont lisibles. Le préfixe se tokenise forcément, l'erreur
            // étant sa première position illisible — sinon on renonce à tout.
            match tokenize(&text[..at]) {
                Ok(prefix) => (prefix, Some((at, message))),
                Err(_) => {
                    units.push(Unit::Unreadable {
                        start: 0,
                        end: text.len(),
                        message,
                    });
                    return units;
                }
            }
        }
    };

    let mut statements = split_tokens(tokens, text, &index);
    if let Some((at, message)) = failure {
        // Le dernier statement du préfixe est coupé par l'erreur : il fait
        // partie de la zone illisible, sauf s'il était terminé par un `;`.
        let unreadable_from = match statements.pop() {
            Some(last) if !last.terminated => last.tokens.start,
            Some(last) => {
                statements.push(last);
                at
            }
            None => 0,
        };
        units.extend(statements.into_iter().map(|s| Unit::Statement(s.tokens)));
        units.push(Unit::Unreadable {
            start: unreadable_from,
            end: text.len(),
            message,
        });
        return units;
    }
    units.extend(statements.into_iter().map(|s| Unit::Statement(s.tokens)));
    units
}

/// Erreur de tokenisation : (offset octet, message).
fn tokenize(text: &str) -> Result<Vec<TokenWithSpan>, (usize, String)> {
    Tokenizer::new(&PgweaveDialect, text)
        .tokenize_with_location()
        .map_err(|e| {
            let at = LineIndex::new(text)
                .offset(text, e.location)
                .unwrap_or(0)
                .min(text.len());
            (at, e.message)
        })
}

struct Split {
    tokens: StatementTokens,
    /// Terminé par un `;` (et non par la fin du texte).
    terminated: bool,
}

fn split_tokens(tokens: Vec<TokenWithSpan>, text: &str, index: &LineIndex) -> Vec<Split> {
    let mut out = Vec::new();
    let mut current: Vec<TokenWithSpan> = Vec::new();
    let mut atomic = AtomicBody::default();

    let finish = |current: &mut Vec<TokenWithSpan>, out: &mut Vec<Split>, terminated: bool| {
        let tokens = std::mem::take(current);
        let first = tokens.iter().position(|t| !is_trivia(&t.token));
        let Some(first) = first else { return };
        let last = tokens.iter().rposition(|t| !is_trivia(&t.token)).unwrap();
        let start = index.offset(text, tokens[first].span.start).unwrap_or(0);
        let end = index.offset(text, tokens[last].span.end).unwrap_or(start);
        out.push(Split {
            tokens: StatementTokens {
                tokens: tokens[first..=last].to_vec(),
                start,
                end,
            },
            terminated,
        });
    };

    for tok in tokens {
        if tok.token == Token::SemiColon && !atomic.inside() {
            finish(&mut current, &mut out, true);
            atomic = AtomicBody::default();
            continue;
        }
        atomic.observe(&tok.token, &current);
        current.push(tok);
    }
    finish(&mut current, &mut out, false);
    out
}

fn is_trivia(t: &Token) -> bool {
    matches!(t, Token::Whitespace(_) | Token::EOF)
}

/// Suivi d'un corps `BEGIN ATOMIC … END` (fonction en SQL standard) : ses
/// statements internes sont séparés par des `;` qui ne terminent pas le
/// `CREATE FUNCTION`. Les `CASE … END` imbriqués sont comptés.
#[derive(Default)]
struct AtomicBody {
    depth: u32,
}

impl AtomicBody {
    fn inside(&self) -> bool {
        self.depth > 0
    }

    fn observe(&mut self, token: &Token, before: &[TokenWithSpan]) {
        let Token::Word(w) = token else { return };
        if self.depth == 0 {
            if w.keyword == Keyword::ATOMIC
                && last_keyword(before) == Some(Keyword::BEGIN)
                && is_routine_definition(before)
            {
                self.depth = 1;
            }
            return;
        }
        match w.keyword {
            Keyword::CASE => self.depth += 1,
            Keyword::END => self.depth -= 1,
            _ => {}
        }
    }
}

fn significant(tokens: &[TokenWithSpan]) -> impl DoubleEndedIterator<Item = &Token> {
    tokens.iter().map(|t| &t.token).filter(|t| !is_trivia(t))
}

fn last_keyword(tokens: &[TokenWithSpan]) -> Option<Keyword> {
    match significant(tokens).next_back() {
        Some(Token::Word(w)) => Some(w.keyword),
        _ => None,
    }
}

/// `CREATE [OR REPLACE] FUNCTION|PROCEDURE …`
fn is_routine_definition(tokens: &[TokenWithSpan]) -> bool {
    let keywords: Vec<Keyword> = significant(tokens)
        .take(4)
        .map(|t| match t {
            Token::Word(w) => w.keyword,
            _ => Keyword::NoKeyword,
        })
        .collect();
    match keywords.as_slice() {
        [Keyword::CREATE, Keyword::OR, Keyword::REPLACE, k, ..] | [Keyword::CREATE, k, ..] => {
            matches!(k, Keyword::FUNCTION | Keyword::PROCEDURE)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<String> {
        split(sql)
            .into_iter()
            .map(|u| match u {
                Unit::Statement(s) => sql[s.start..s.end].to_string(),
                Unit::Unreadable { start, end, .. } => format!("!{}", &sql[start..end]),
            })
            .collect()
    }

    #[test]
    fn decoupe_sur_les_points_virgules_de_premier_niveau() {
        assert_eq!(
            texts("-- tête\nSELECT 1;  SELECT ';' , $$ ; $$ ;\n/* fin */ SELECT 3"),
            ["SELECT 1", "SELECT ';' , $$ ; $$", "SELECT 3"]
        );
    }

    #[test]
    fn statements_vides_ignores() {
        assert_eq!(texts(";; -- rien\n ;"), Vec::<String>::new());
    }

    #[test]
    fn begin_atomic_garde_ses_points_virgules() {
        let sql = "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT CASE WHEN true THEN 1 END; SELECT 2; END; SELECT 4";
        assert_eq!(
            texts(sql),
            [
                "CREATE FUNCTION f() RETURNS int LANGUAGE sql BEGIN ATOMIC SELECT CASE WHEN true THEN 1 END; SELECT 2; END",
                "SELECT 4"
            ]
        );
    }

    #[test]
    fn begin_transaction_n_est_pas_atomic() {
        assert_eq!(
            texts("BEGIN; SELECT 1; COMMIT"),
            ["BEGIN", "SELECT 1", "COMMIT"]
        );
    }

    #[test]
    fn une_chaine_non_terminee_n_emporte_que_la_fin() {
        let t = texts("SELECT 1; SELECT 'oups; SELECT 3;");
        assert_eq!(t[0], "SELECT 1");
        assert!(t[1].starts_with("!SELECT 'oups"), "{t:?}");
        assert_eq!(t.len(), 2);
    }
}
