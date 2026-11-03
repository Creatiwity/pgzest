//! Minimal lossless lexing of PostgreSQL SQL: every byte belongs to a token.
//! Enough to tell code apart from strings, comments and quoted identifiers —
//! what is needed to recognize an invocation or a qualifier without mistaking
//! the context.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Whitespace,
    /// `-- …` up to the end of line (excluded).
    LineComment,
    /// `/* … */`, nesting included (like PostgreSQL).
    BlockComment,
    /// `'…'`, `E'…'`, `$tag$…$tag$`: content never interpreted.
    String,
    /// `"…"`.
    QuotedIdent,
    /// Word: identifier or keyword.
    Word,
    /// `$name` or `$1`.
    Param,
    Number,
    /// Any other character, one per token.
    Punct,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Token {
    pub kind: Kind,
    pub range: Range<usize>,
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Tokens of `src`, contiguous from 0 to `src.len()`. An unterminated string or
/// comment extends to the end.
pub(crate) fn tokenize(src: &str) -> Vec<Token> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < src.len() {
        let c = src[i..].chars().next().unwrap_or('\0');
        let start = i;
        let kind = if c.is_whitespace() {
            i += src[i..]
                .find(|c: char| !c.is_whitespace())
                .unwrap_or(src.len() - i);
            Kind::Whitespace
        } else if src[i..].starts_with("--") {
            i += src[i..].find('\n').unwrap_or(src.len() - i);
            Kind::LineComment
        } else if src[i..].starts_with("/*") {
            let mut depth = 0usize;
            while i < src.len() {
                if src[i..].starts_with("/*") {
                    depth += 1;
                    i += 2;
                } else if src[i..].starts_with("*/") {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            Kind::BlockComment
        } else if c == '\'' || ((c == 'E' || c == 'e') && b.get(i + 1) == Some(&b'\'')) {
            let escapes = c != '\'';
            i += if escapes { 2 } else { 1 };
            while i < src.len() {
                match b[i] {
                    b'\\' if escapes => i += 2,
                    b'\'' if b.get(i + 1) == Some(&b'\'') => i += 2,
                    b'\'' => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
            Kind::String
        } else if c == '"' {
            i += 1;
            while i < src.len() {
                if b[i] == b'"' {
                    if b.get(i + 1) == Some(&b'"') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            Kind::QuotedIdent
        } else if c == '$' {
            // `$tag$` opens a string; `$name`, `$1` are parameters.
            let rest = &src[i + 1..];
            let tag_len = rest
                .find(|c: char| !(c.is_alphanumeric() || c == '_'))
                .unwrap_or(rest.len());
            let tag_ok = rest[..tag_len]
                .chars()
                .next()
                .is_none_or(|c| !c.is_ascii_digit());
            if tag_ok && rest[tag_len..].starts_with('$') {
                let delim = &src[i..i + tag_len + 2];
                let body = i + delim.len();
                i = match src[body..].find(delim) {
                    Some(k) => body + k + delim.len(),
                    None => src.len(),
                };
                Kind::String
            } else if tag_len > 0 {
                i += 1 + tag_len;
                Kind::Param
            } else {
                i += 1;
                Kind::Punct
            }
        } else if is_word_start(c) {
            i += src[i..]
                .find(|c: char| !is_word_char(c))
                .unwrap_or(src.len() - i);
            Kind::Word
        } else if c.is_ascii_digit() {
            i += src[i..]
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_'))
                .unwrap_or(src.len() - i);
            Kind::Number
        } else {
            i += c.len_utf8();
            Kind::Punct
        };
        out.push(Token {
            kind,
            range: start..i,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(Kind, &str)> {
        tokenize(src)
            .into_iter()
            .filter(|t| t.kind != Kind::Whitespace)
            .map(|t| (t.kind, &src[t.range]))
            .collect()
    }

    #[test]
    fn strings_comments_and_quoted_identifiers() {
        let src = "SELECT 'a@b', E'x\\'y', $f$ @c $f$, \"@d\" -- @e\n/* /* @f */ */ @g $p $1";
        assert_eq!(
            kinds(src),
            vec![
                (Kind::Word, "SELECT"),
                (Kind::String, "'a@b'"),
                (Kind::Punct, ","),
                (Kind::String, "E'x\\'y'"),
                (Kind::Punct, ","),
                (Kind::String, "$f$ @c $f$"),
                (Kind::Punct, ","),
                (Kind::QuotedIdent, "\"@d\""),
                (Kind::LineComment, "-- @e"),
                (Kind::BlockComment, "/* /* @f */ */"),
                (Kind::Punct, "@"),
                (Kind::Word, "g"),
                (Kind::Param, "$p"),
                (Kind::Param, "$1"),
            ]
        );
    }

    #[test]
    fn tokens_tile_the_source() {
        let src = "a.b, 'x'  -- c\n\"q\"\"r\" $$ z $$";
        let toks = tokenize(src);
        let mut at = 0;
        for t in &toks {
            assert_eq!(t.range.start, at);
            at = t.range.end;
        }
        assert_eq!(at, src.len());
    }
}
