//! Queries with variants (P6): conditional blocks `[[ … ]]` and choices
//! `{{ name: a = … | b = … }}`.
//!
//! ```sql
//! SELECT o.id FROM orders o
//! WHERE o.restaurant_id = $restaurant
//! [[ AND o.status = $status ]]
//! ORDER BY {{ sort: recent = o.placed_at DESC | id = o.id }}
//! ```
//!
//! - A block depends on a single named parameter, optional in Rust: it is
//!   included if the value is provided.
//! - A choice becomes a Rust enum (`sort`, variants `recent`, `id`).
//! - Each combination is a variant: a text of the **same length** as the
//!   query, with excluded text and markers replaced by spaces. A position in
//!   a variant is therefore a position in the file.
//! - All variants are described at compile time and must return the same
//!   columns; at run time, the generated function picks one of them. No SQL
//!   is composed at run time.

use std::ops::Range;

use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};

use crate::line_index::LineIndex;

/// Maximum number of variants of a query.
pub const MAX_VARIANTS: usize = 256;

/// Bloc `[[ … ]]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// From the first `[` to the last `]`.
    pub range: Range<usize>,
    pub inner: Range<usize>,
}

/// One option of a choice: `label = body`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alternative {
    pub label: String,
    /// `label =`, blanked in every variant.
    pub head: Range<usize>,
    pub body: Range<usize>,
}

/// Choice `{{ name: … | … }}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub name: String,
    pub range: Range<usize>,
    /// `{{ name:`, blanked.
    pub head: Range<usize>,
    pub alternatives: Vec<Alternative>,
    /// `|` separators and the final `}}`, blanked.
    pub marks: Vec<Range<usize>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Structure {
    pub blocks: Vec<Block>,
    pub choices: Vec<Choice>,
}

impl Structure {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty() && self.choices.is_empty()
    }
}

/// Structure error, positioned in the analyzed text.
pub type StructureError = (Range<usize>, String);

fn snake(s: &str) -> bool {
    let mut b = s.bytes();
    matches!(b.next(), Some(b'a'..=b'z' | b'_'))
        && b.all(|c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

/// Blocks and choices of `sql`. Nothing is recognized inside a string or a
/// comment; `[[` after an expression (array subscript, `ARRAY[[`) does not
/// open a block.
pub fn structure(sql: &str) -> Result<Structure, StructureError> {
    let index = LineIndex::new(sql);
    let Ok(raw) = Tokenizer::new(&PostgreSqlDialect {}, sql)
        .with_unescape(false)
        .tokenize_with_location()
    else {
        // Unreadable: parsing will report it.
        return Ok(Structure::default());
    };
    let toks: Vec<(Token, usize)> = raw
        .into_iter()
        .filter(|t| t.token != Token::EOF)
        .filter_map(|t| Some((t.token, index.offset(t.span.start)?)))
        .collect();
    let len = |t: &Token, at: usize, next: Option<usize>| {
        next.unwrap_or(sql.len())
            .min(at + t.to_string().len().max(1))
    };
    let end_of = |i: usize| len(&toks[i].0, toks[i].1, toks.get(i + 1).map(|t| t.1));
    let significant = |t: &Token| !matches!(t, Token::Whitespace(_));
    let adjacent = |i: usize, a: Token, b: Token| {
        toks.get(i).is_some_and(|t| t.0 == a)
            && toks
                .get(i + 1)
                .is_some_and(|t| t.0 == b && t.1 == toks[i].1 + 1)
    };

    let mut out = Structure::default();
    let mut i = 0;
    while i < toks.len() {
        let (tok, at) = (&toks[i].0, toks[i].1);
        if adjacent(i, Token::LBracket, Token::LBracket) {
            // Array subscript or constructor: `a[[`, `ARRAY[[`. The `]]`
            // closing a previous block is not one.
            let prev = toks[..i]
                .iter()
                .rev()
                .find(|t| significant(&t.0))
                .filter(|t| !out.blocks.iter().any(|b| b.range.end == t.1 + 1))
                .map(|t| &t.0);
            let subscript = match prev {
                Some(Token::Word(w)) => {
                    w.quote_style.is_some()
                        || w.value.eq_ignore_ascii_case("array")
                        || !matches!(
                            w.keyword,
                            sqlparser::keywords::Keyword::AND
                                | sqlparser::keywords::Keyword::OR
                                | sqlparser::keywords::Keyword::WHERE
                                | sqlparser::keywords::Keyword::BY
                                | sqlparser::keywords::Keyword::SELECT
                                | sqlparser::keywords::Keyword::ON
                                | sqlparser::keywords::Keyword::HAVING
                                | sqlparser::keywords::Keyword::NOT
                        ) && w.keyword == sqlparser::keywords::Keyword::NoKeyword
                }
                Some(Token::RBracket | Token::RParen) => true,
                _ => false,
            };
            if subscript {
                i += 1;
                continue;
            }
            // End: `]]` at the same bracket depth.
            let mut depth = 0i32;
            let mut j = i + 2;
            let close = loop {
                let Some((t, _)) = toks.get(j) else {
                    return Err((at..at + 2, "block `[[` without `]]`".into()));
                };
                match t {
                    Token::LBracket
                        if adjacent(j, Token::LBracket, Token::LBracket) && depth == 0 =>
                    {
                        return Err((
                            toks[j].1..toks[j].1 + 2,
                            "nested `[[ … ]]` blocks: not supported".into(),
                        ));
                    }
                    Token::LBracket => depth += 1,
                    Token::RBracket
                        if depth == 0 && adjacent(j, Token::RBracket, Token::RBracket) =>
                    {
                        break j;
                    }
                    Token::RBracket => depth -= 1,
                    Token::LBrace if adjacent(j, Token::LBrace, Token::LBrace) => {
                        return Err((
                            toks[j].1..toks[j].1 + 2,
                            "choice `{{` inside a block: not supported".into(),
                        ));
                    }
                    _ => {}
                }
                j += 1;
            };
            let end = toks[close].1 + 2;
            out.blocks.push(Block {
                range: at..end,
                inner: at + 2..toks[close].1,
            });
            i = close + 2;
            continue;
        }
        if adjacent(i, Token::LBrace, Token::LBrace) {
            let mut j = i + 2;
            let skip_ws = |j: &mut usize| {
                while toks.get(*j).is_some_and(|t| {
                    matches!(
                        t.0,
                        Token::Whitespace(
                            Whitespace::Space | Whitespace::Newline | Whitespace::Tab
                        )
                    )
                }) {
                    *j += 1;
                }
            };
            skip_ws(&mut j);
            let name = match toks.get(j) {
                Some((Token::Word(w), _)) if w.quote_style.is_none() && snake(&w.value) => {
                    w.value.clone()
                }
                _ => {
                    return Err((
                        at..at + 2,
                        "choice: `{{ name: a = … | b = … }}`, name in snake_case".into(),
                    ));
                }
            };
            j += 1;
            skip_ws(&mut j);
            if toks.get(j).map(|t| &t.0) != Some(&Token::Colon) {
                return Err((
                    at..at + 2,
                    format!("choice `{name}`: `:` expected after the name"),
                ));
            }
            j += 1;
            let head = at..toks.get(j).map_or(sql.len(), |t| t.1);
            let mut alternatives = Vec::new();
            let mut marks = Vec::new();
            let mut depth = 0i32;
            // Current alternative: (start of `label =`).
            let mut alt_start = j;
            let close = loop {
                let Some((t, tat)) = toks.get(j) else {
                    return Err((at..at + 2, format!("choice `{name}` without `}}}}`")));
                };
                let boundary = depth == 0
                    && (matches!(t, Token::Pipe)
                        || (matches!(t, Token::RBrace)
                            && adjacent(j, Token::RBrace, Token::RBrace)));
                if boundary {
                    // `label = body` between alt_start and j.
                    let mut k = alt_start;
                    skip_ws(&mut k);
                    let label = match toks.get(k) {
                        Some((Token::Word(w), _)) if w.quote_style.is_none() && snake(&w.value) => {
                            w.value.clone()
                        }
                        _ => {
                            return Err((
                                toks.get(k).map_or(*tat, |t| t.1)..*tat,
                                format!("choice `{name}`: each option is written `name = SQL`"),
                            ));
                        }
                    };
                    let label_at = toks[k].1;
                    k += 1;
                    skip_ws(&mut k);
                    if toks.get(k).map(|t| &t.0) != Some(&Token::Eq) {
                        return Err((
                            label_at..label_at + label.len(),
                            format!("choice `{name}`: `=` expected after `{label}`"),
                        ));
                    }
                    let body_start = toks.get(k + 1).map_or(*tat, |t| t.1);
                    alternatives.push(Alternative {
                        label,
                        head: label_at..body_start,
                        body: body_start..*tat,
                    });
                    if matches!(t, Token::Pipe) {
                        marks.push(*tat..end_of(j));
                        alt_start = j + 1;
                    } else {
                        break j;
                    }
                }
                match t {
                    Token::LParen | Token::LBracket | Token::LBrace => depth += 1,
                    Token::RParen | Token::RBracket | Token::RBrace => depth -= 1,
                    Token::Placeholder(p) if p.starts_with('$') => {
                        return Err((
                            *tat..*tat + p.len(),
                            format!("choice `{name}`: no parameter allowed in an option"),
                        ));
                    }
                    _ => {}
                }
                j += 1;
            };
            let end = toks[close].1 + 2;
            marks.push(toks[close].1..end);
            if alternatives.len() < 2 {
                return Err((at..end, format!("choice `{name}`: at least two options")));
            }
            for (k, a) in alternatives.iter().enumerate() {
                if alternatives[..k].iter().any(|b| b.label == a.label) {
                    return Err((
                        a.head.clone(),
                        format!("choice `{name}`: duplicate `{}`", a.label),
                    ));
                }
            }
            if out.choices.iter().any(|c| c.name == name) {
                return Err((at..end, format!("duplicate choice `{name}`")));
            }
            out.choices.push(Choice {
                name,
                range: at..end,
                head,
                alternatives,
                marks,
            });
            i = close + 2;
            continue;
        }
        let _ = tok;
        i += 1;
    }
    Ok(out)
}

/// Combination: included blocks, selected option of each choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub blocks: Vec<bool>,
    pub choices: Vec<usize>,
}

/// Text of variant `sel`: same length as `sql`, with excluded text and
/// markers blanked (line breaks kept).
pub fn render(sql: &str, s: &Structure, sel: &Selection) -> String {
    let mut b = sql.as_bytes().to_vec();
    let mut blank = |r: &Range<usize>| {
        for c in &mut b[r.clone()] {
            if *c != b'\n' {
                *c = b' ';
            }
        }
    };
    for (block, keep) in s.blocks.iter().zip(&sel.blocks) {
        if *keep {
            blank(&(block.range.start..block.inner.start));
            blank(&(block.inner.end..block.range.end));
        } else {
            blank(&block.range);
        }
    }
    for (choice, picked) in s.choices.iter().zip(&sel.choices) {
        blank(&choice.head);
        for m in &choice.marks {
            blank(m);
        }
        for (k, a) in choice.alternatives.iter().enumerate() {
            blank(&a.head);
            if k != *picked {
                blank(&a.body);
            }
        }
    }
    String::from_utf8(b).expect("only bytes replaced by spaces")
}

/// All combinations, in index order: blocks in the low bits (block 0 =
/// bit 0), then choices in mixed radix.
pub fn selections(s: &Structure) -> Vec<Selection> {
    let n_blocks = s.blocks.len();
    let radices: Vec<usize> = s.choices.iter().map(|c| c.alternatives.len()).collect();
    let total = (1usize << n_blocks) * radices.iter().product::<usize>();
    (0..total)
        .map(|mut idx| {
            let blocks = (0..n_blocks).map(|k| idx >> k & 1 == 1).collect();
            idx >>= n_blocks;
            let choices = radices
                .iter()
                .map(|r| {
                    let v = idx % r;
                    idx /= r;
                    v
                })
                .collect();
            Selection { blocks, choices }
        })
        .collect()
}

/// Number of variants, without enumerating them.
pub fn variant_count(s: &Structure) -> usize {
    s.choices.iter().fold(
        1usize
            .checked_shl(s.blocks.len() as u32)
            .unwrap_or(usize::MAX),
        |n, c| n.saturating_mul(c.alternatives.len()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const Q: &str = "SELECT o.id FROM orders o WHERE o.r = $r [[ AND o.s = $s ]]\n[[ AND o.t = $t ]]\nORDER BY {{ sort: recent = o.at DESC | id = o.id }}";

    #[test]
    fn structure_and_variants() {
        let s = structure(Q).unwrap();
        assert_eq!(s.blocks.len(), 2);
        assert_eq!(&Q[s.blocks[0].inner.clone()], " AND o.s = $s ");
        assert_eq!(s.choices[0].name, "sort");
        let labels: Vec<_> = s.choices[0]
            .alternatives
            .iter()
            .map(|a| a.label.as_str())
            .collect();
        assert_eq!(labels, vec!["recent", "id"]);
        let all = selections(&s);
        assert_eq!(all.len(), 8);
        let squash = |t: String| t.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            squash(render(Q, &s, &all[0])),
            "SELECT o.id FROM orders o WHERE o.r = $r ORDER BY o.at DESC"
        );
        assert_eq!(
            squash(render(Q, &s, &all[7])),
            "SELECT o.id FROM orders o WHERE o.r = $r AND o.s = $s AND o.t = $t ORDER BY o.id"
        );
        for sel in &all {
            assert_eq!(render(Q, &s, sel).len(), Q.len());
        }
    }

    #[test]
    fn arrays_and_strings_are_not_blocks() {
        for sql in [
            "SELECT ARRAY[[1,2],[3,4]]",
            "SELECT a[[1]] FROM t",
            "SELECT '[[ x ]]', '{{ a: b = c | d = e }}' -- [[ ]]",
        ] {
            assert!(structure(sql).unwrap().is_empty(), "{sql}");
        }
    }

    #[test]
    fn structure_errors() {
        for (sql, expected) in [
            ("SELECT 1 WHERE true [[ AND x = $x", "without `]]`"),
            ("SELECT 1 ORDER BY {{ sort: a = x }}", "two options"),
            ("SELECT 1 ORDER BY {{ sort: a = x | a = y }}", "duplicate"),
            (
                "SELECT 1 ORDER BY {{ sort: a = $x | b = y }}",
                "no parameter",
            ),
            ("SELECT 1 ORDER BY {{ Sort: a = x | b = y }}", "snake_case"),
            ("SELECT 1 WHERE true [[ AND [[ x ]] ]]", "nested"),
        ] {
            let e = structure(sql).unwrap_err();
            assert!(e.1.contains(expected), "{sql}: {}", e.1);
        }
    }
}
