//! Splitting a text into statements, on tokens.
//!
//! The whole text is tokenized once by pgweave's dialect: strings,
//! dollar strings, quoted identifiers and comments are
//! atomic tokens, so a `;` they contain never cuts a
//! statement. Token positions are those of the text: the parse of
//! each statement reuses them as is, without copying or shifting.
//!
//! Replaces a byte-by-byte textual split, which had to reimplement
//! PostgreSQL's lexical rules by hand (and got the rare cases wrong).

use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer};

use crate::dialect::PgweaveDialect;

/// A piece of the text: a statement to parse, or a region the tokenizer
/// could not read.
pub(crate) enum Unit {
    Statement(StatementTokens),
    /// Unreadable text up to the end (unterminated string…). The complete
    /// statements preceding it were split normally.
    Unreadable {
        start: usize,
        end: usize,
        message: String,
    },
}

pub(crate) struct StatementTokens {
    /// Tokens of the statement, without the final `;` or the whitespace preceding it.
    pub tokens: Vec<TokenWithSpan>,
    /// Byte offset of the first significant token, in the text.
    pub start: usize,
    /// End byte offset of the last significant token.
    pub end: usize,
}

/// Index of line starts, to translate a `Location` into a byte offset.
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

    /// Byte offset of a sqlparser position (1-based line and column,
    /// column in characters).
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

/// Splits `text` into statements.
pub(crate) fn split(text: &str) -> Vec<Unit> {
    let index = LineIndex::new(text);
    let mut units = Vec::new();

    let (tokens, failure) = match tokenize(text) {
        Ok(tokens) => (tokens, None),
        Err((at, message)) => {
            // Keep what precedes the error: the complete statements before
            // it are readable. The prefix necessarily tokenizes, the error
            // being its first unreadable position — otherwise we give up on everything.
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
        // The last statement of the prefix is cut by the error: it is
        // part of the unreadable region, unless it was terminated by a `;`.
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

/// Tokenization error: (byte offset, message).
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
    /// Terminated by a `;` (and not by the end of the text).
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

/// Tracking of a `BEGIN ATOMIC … END` body (standard SQL function): its
/// inner statements are separated by `;` that do not terminate the
/// `CREATE FUNCTION`. Nested `CASE … END` are counted.
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
    fn splits_on_top_level_semicolons() {
        assert_eq!(
            texts("-- head\nSELECT 1;  SELECT ';' , $$ ; $$ ;\n/* end */ SELECT 3"),
            ["SELECT 1", "SELECT ';' , $$ ; $$", "SELECT 3"]
        );
    }

    #[test]
    fn empty_statements_ignored() {
        assert_eq!(texts(";; -- nothing\n ;"), Vec::<String>::new());
    }

    #[test]
    fn begin_atomic_keeps_its_semicolons() {
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
    fn begin_transaction_is_not_atomic() {
        assert_eq!(
            texts("BEGIN; SELECT 1; COMMIT"),
            ["BEGIN", "SELECT 1", "COMMIT"]
        );
    }

    #[test]
    fn an_unterminated_string_only_takes_the_end() {
        let t = texts("SELECT 1; SELECT 'oups; SELECT 3;");
        assert_eq!(t[0], "SELECT 1");
        assert!(t[1].starts_with("!SELECT 'oups"), "{t:?}");
        assert_eq!(t.len(), 2);
    }
}
