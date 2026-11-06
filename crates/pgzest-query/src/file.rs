//! Named-query file format (`site/src/content/docs/reference/named-queries.md`).
//!
//! Splitting relies on the sqlparser tokenizer, not on a line scan: a
//! `-- name:` inside a string or a `$$…$$` body is not a header, and a
//! `$name` inside a string is not a parameter. The tokenizer also emits
//! whitespace and comments, so tokens tile the source: the end of a token is
//! the start of the next one.

use std::collections::HashMap;
use std::ops::Range;

use pgzest_diagnostics::{Diagnostic, Span, codes};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};

use crate::line_index::LineIndex;

/// Number of rows returned, declared in the header (`-- name: x :one`).
///
/// Optional: since P4, cardinality is inferred and the declaration acts as a
/// contract checked against the inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Cardinality {
    /// Exactly one row.
    One,
    /// Zero or one row.
    Optional,
    /// Zero, one or more rows.
    Many,
    /// No row returned (statement without `RETURNING`).
    None,
}

impl Cardinality {
    pub const ALL: [Cardinality; 4] = [
        Cardinality::One,
        Cardinality::Optional,
        Cardinality::Many,
        Cardinality::None,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Cardinality::One => "one",
            Cardinality::Optional => "optional",
            Cardinality::Many => "many",
            Cardinality::None => "none",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

/// Named parameter and its occurrences in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedParam {
    pub name: String,
    /// Byte ranges of each `$name` in the file, in order; an occurrence
    /// marked `$name?` includes the `?`.
    pub occurrences: Vec<Range<usize>>,
    /// `$name?`: the value may be absent (NULL, `Option` in Rust), and the
    /// query is written for it (`$name::type IS NULL OR …`).
    pub optional: bool,
}

/// Parameters of a query. The two notations do not mix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Params {
    None,
    /// `$1 … $n`, without gaps.
    Positional {
        count: u32,
    },
    /// `$name`, in order of first appearance — this is the order of the
    /// positional parameters after rewriting.
    Named(Vec<NamedParam>),
}

/// SQL of a query after its fragments are inserted (P6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpandedSql {
    pub sql: String,
    /// Positions in the inserted SQL → positions in the file.
    pub map: pgzest_ast::SourceMap,
}

/// Structure of a query with variants (see [`crate::dynamic`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dynamic {
    /// Positions in [`NamedQuery::text`].
    pub structure: crate::dynamic::Structure,
    /// Parameter deciding each block, in block order.
    pub conditions: Vec<String>,
}

/// A named query, as written in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedQuery {
    pub name: String,
    /// Range of the name in the header.
    pub name_range: Range<usize>,
    pub cardinality: Option<Cardinality>,
    /// `-> other` (same file) or `-> a::b::other` (from the root of the
    /// queries directory): rows have the shape of that query's rows (same
    /// generated struct), and its range in the header.
    pub row: Option<(String, Range<usize>)>,
    /// Comment lines between the header and the SQL, without the `-- `.
    pub doc: Vec<String>,
    /// The statement, without the trailing `;`.
    pub sql: String,
    /// Range of `sql` in the file.
    pub sql_range: Range<usize>,
    /// Fragments inserted: the SQL analyzed and sent. `None` without fragments.
    pub expanded: Option<ExpandedSql>,
    /// `[[ … ]]` blocks and `{{ … }}` choices: query with variants.
    pub dynamic: Option<Dynamic>,
    /// Parameters of the sent SQL. With fragments, their occurrences are
    /// positions in [`ExpandedSql::sql`] shifted by `sql_range.start`;
    /// otherwise, positions in the file.
    pub params: Params,
}

impl NamedQuery {
    /// Analyzed SQL: fragments inserted if any.
    pub fn text(&self) -> &str {
        self.expanded.as_ref().map_or(&self.sql, |e| &e.sql)
    }

    /// [`Self::text`], with the `?` markers of optional parameters replaced
    /// by a space: SQL readable by the parser, positions unchanged. For a
    /// query with variants, the reference variant (all blocks, the first
    /// option of each choice).
    pub fn analysis_text(&self) -> String {
        match &self.dynamic {
            None => self.unmarked_text(),
            Some(d) => self.variant(&reference_selection(&d.structure)).0,
        }
    }

    /// Variant `sel`: analyzed text (same length as [`Self::text`]),
    /// positional SQL sent, and parameter names in `$1…$n` order.
    pub fn variant(&self, sel: &crate::dynamic::Selection) -> (String, String, Vec<String>) {
        let text = match &self.dynamic {
            None => self.unmarked_text(),
            Some(d) => crate::dynamic::render(&self.unmarked_text(), &d.structure, sel),
        };
        let base = self.sql_range.start;
        let excluded = |r: &Range<usize>| {
            self.dynamic.as_ref().is_some_and(|d| {
                d.structure
                    .blocks
                    .iter()
                    .zip(&sel.blocks)
                    .any(|(b, keep)| !keep && b.inner.contains(&(r.start - base)))
            })
        };
        let Params::Named(params) = &self.params else {
            return (text.clone(), text, Vec::new());
        };
        let mut kept: Vec<(Range<usize>, usize)> = Vec::new();
        let mut order: Vec<String> = Vec::new();
        let mut occurrences: Vec<(&Range<usize>, &str)> = params
            .iter()
            .flat_map(|p| p.occurrences.iter().map(move |r| (r, p.name.as_str())))
            .filter(|(r, _)| !excluded(r))
            .collect();
        occurrences.sort_by_key(|(r, _)| r.start);
        for (r, name) in occurrences {
            let n = match order.iter().position(|o| o == name) {
                Some(k) => k + 1,
                None => {
                    order.push(name.to_string());
                    order.len()
                }
            };
            kept.push((r.clone(), n));
        }
        let mut sql = String::with_capacity(text.len());
        let mut cursor = 0;
        for (r, n) in &kept {
            sql.push_str(&text[cursor..r.start - base]);
            sql.push_str(&format!("${n}"));
            cursor = r.end - base;
        }
        sql.push_str(&text[cursor..]);
        (text, sql, order)
    }

    fn unmarked_text(&self) -> String {
        let mut bytes = self.text().as_bytes().to_vec();
        if let Params::Named(params) = &self.params {
            for r in params.iter().flat_map(|p| &p.occurrences) {
                let end = r.end - self.sql_range.start;
                if bytes.get(end.wrapping_sub(1)) == Some(&b'?') {
                    bytes[end - 1] = b' ';
                }
            }
        }
        String::from_utf8(bytes).expect("a `?` replaced by a space")
    }

    /// The SQL to send to the server: each `$name` replaced by `$n`, `n`
    /// being the rank of the name's first appearance. Unchanged for a
    /// positional query or one without parameters.
    pub fn positional_sql(&self) -> String {
        if let Some(d) = &self.dynamic {
            return self.variant(&reference_selection(&d.structure)).1;
        }
        let text = self.text();
        let Params::Named(params) = &self.params else {
            return text.to_string();
        };
        let mut replacements: Vec<(Range<usize>, usize)> = params
            .iter()
            .enumerate()
            .flat_map(|(i, p)| p.occurrences.iter().map(move |r| (r.clone(), i + 1)))
            .collect();
        replacements.sort_by_key(|(r, _)| r.start);

        let base = self.sql_range.start;
        let mut out = String::with_capacity(text.len());
        let mut cursor = 0;
        for (range, n) in replacements {
            out.push_str(&text[cursor..range.start - base]);
            out.push_str(&format!("${n}"));
            cursor = range.end - base;
        }
        out.push_str(&text[cursor..]);
        out
    }
}

/// Result of splitting a file.
#[derive(Debug, Clone)]
pub struct QueryFile {
    /// Valid queries, in file order. A query with an invalid header is not
    /// included (it produces a diagnostic).
    pub queries: Vec<NamedQuery>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Splits a named-query file and checks its format. Without fragments: an
/// `@name` stays as is (see [`parse_query_file_with`]).
pub fn parse_query_file(source: &str) -> QueryFile {
    parse_query_file_impl(source, None)
}

/// Like [`parse_query_file`], with the fragments of `macros` inserted into
/// each query (P6). Diagnostics stay positioned in the file.
pub fn parse_query_file_with(source: &str, macros: &pgzest_expand::MacroSet) -> QueryFile {
    parse_query_file_impl(source, Some(macros))
}

fn parse_query_file_impl(source: &str, macros: Option<&pgzest_expand::MacroSet>) -> QueryFile {
    let mut diagnostics = Vec::new();
    let index = LineIndex::new(source);

    let tokens = match Tokenizer::new(&PostgreSqlDialect {}, source)
        .with_unescape(false)
        .tokenize_with_location()
    {
        Ok(t) => t,
        Err(e) => {
            let at = index.offset(e.location).unwrap_or(0);
            diagnostics.push(
                Diagnostic::error(
                    codes::PARSE_ERROR,
                    format!("unreadable query file: {}", e.message),
                )
                .with_span(span(at..at)),
            );
            return QueryFile {
                queries: Vec::new(),
                diagnostics,
            };
        }
    };

    let tokens: Vec<_> = tokens
        .into_iter()
        .filter(|t| t.token != Token::EOF)
        .collect();
    let starts: Vec<usize> = tokens
        .iter()
        .map(|t| index.offset(t.span.start).unwrap_or(source.len()))
        .collect();

    let mut parser = FileParser {
        source,
        macros,
        diagnostics,
        queries: Vec::new(),
        current: None,
        outside_reported: false,
    };

    for (i, tok) in tokens.iter().enumerate() {
        let range = starts[i]..starts.get(i + 1).copied().unwrap_or(source.len());
        let line_start = tok.span.start.column == 1;
        parser.token(&tok.token, range, line_start);
    }
    parser.finish_current();

    let FileParser {
        mut queries,
        mut diagnostics,
        ..
    } = parser;
    report_duplicates(&mut queries, &mut diagnostics);
    QueryFile {
        queries,
        diagnostics,
    }
}

// ── State machine ─────────────────────────────────────────────────────────────

struct FileParser<'a> {
    source: &'a str,
    macros: Option<&'a pgzest_expand::MacroSet>,
    diagnostics: Vec<Diagnostic>,
    queries: Vec<NamedQuery>,
    current: Option<Builder>,
    /// SQL outside a query is reported only once per file.
    outside_reported: bool,
}

/// Query being read.
struct Builder {
    /// `None` if the header is invalid: the body is consumed (so that it is
    /// not reported as SQL outside a query) but the query is not returned.
    name: Option<(String, Range<usize>)>,
    header_range: Range<usize>,
    cardinality: Option<Cardinality>,
    row: Option<(String, Range<usize>)>,
    doc: Vec<String>,
    body: Option<Range<usize>>,
    after_semicolon: bool,
    extra_reported: bool,
    placeholders: Vec<(String, Range<usize>)>,
}

impl FileParser<'_> {
    fn token(&mut self, token: &Token, range: Range<usize>, line_start: bool) {
        match token {
            Token::Whitespace(Whitespace::SingleLineComment { comment, prefix })
                if prefix == "--" =>
            {
                if let Some(rest) = header_payload(comment) {
                    if line_start {
                        self.header(rest, range);
                    } else {
                        self.diagnostics.push(
                            Diagnostic::error(
                                codes::QUERY_HEADER_INVALID,
                                "a `-- name:` header must start the line",
                            )
                            .with_span(span(range)),
                        );
                    }
                } else if let Some(b) = self.current.as_mut()
                    && b.body.is_none()
                {
                    b.doc.push(doc_line(comment));
                }
            }
            Token::Whitespace(_) => {}
            Token::SemiColon => match self.current.as_mut() {
                Some(b) => b.after_semicolon = true,
                None => self.outside(range),
            },
            other => {
                let Some(b) = self.current.as_mut() else {
                    self.outside(range);
                    return;
                };
                if b.after_semicolon {
                    if !b.extra_reported {
                        b.extra_reported = true;
                        self.diagnostics.push(
                            Diagnostic::error(
                                codes::QUERY_MULTIPLE_STATEMENTS,
                                "a named query contains a single statement",
                            )
                            .with_span(span(range))
                            .with_hint("add a `-- name: …` header before this statement"),
                        );
                    }
                    return;
                }
                match &mut b.body {
                    Some(body) => body.end = range.end,
                    None => b.body = Some(range.clone()),
                }
                if let Token::Placeholder(p) = other
                    && p.starts_with('$')
                {
                    b.placeholders.push((p.clone(), range));
                }
            }
        }
    }

    fn header(&mut self, payload: &str, range: Range<usize>) {
        self.finish_current();

        // Position of the payload in the file, to target the name precisely.
        let payload_start = range.start + self.source[range.clone()].find(payload).unwrap_or(0);
        let mut words = payload.split_whitespace().peekable();
        let name_word = words.next();
        let marker = words.next_if(|w| w.starts_with(':'));
        let arrow = words.next_if(|w| *w == "->");
        let row_word = arrow.and_then(|_| words.next());
        let extra = words.next();

        let mut builder = Builder {
            name: None,
            header_range: range.clone(),
            cardinality: None,
            row: None,
            doc: Vec::new(),
            body: None,
            after_semicolon: false,
            extra_reported: false,
            placeholders: Vec::new(),
        };

        match name_word {
            None => self.diagnostics.push(
                Diagnostic::error(codes::QUERY_HEADER_INVALID, "header without a query name")
                    .with_span(span(range.clone()))
                    .with_hint("expected form: `-- name: query_name [:one|:optional|:many|:none]`"),
            ),
            Some(name) => {
                let start = payload_start + payload.find(name).unwrap_or(0);
                let name_range = start..start + name.len();
                if is_snake_ident(name) {
                    builder.name = Some((name.to_string(), name_range));
                } else {
                    self.diagnostics.push(
                        Diagnostic::error(
                            codes::QUERY_HEADER_INVALID,
                            format!("invalid query name: `{name}`"),
                        )
                        .with_span(span(name_range))
                        .with_hint(
                            "ASCII snake_case: lowercase letters, digits, `_`, no leading digit",
                        ),
                    );
                }
            }
        }

        if let Some(m) = marker {
            match m.strip_prefix(':').and_then(Cardinality::parse) {
                Some(c) => builder.cardinality = Some(c),
                None => self.diagnostics.push(
                    Diagnostic::error(
                        codes::QUERY_HEADER_INVALID,
                        format!("unknown cardinality: `{m}`"),
                    )
                    .with_span(span(range.clone()))
                    .with_hint("values: :one, :optional, :many, :none"),
                ),
            }
        }
        match (arrow, row_word) {
            (Some(_), Some(r)) if r.split("::").all(is_snake_ident) => {
                let after = payload.find("->").map_or(0, |k| k + 2);
                let start = payload_start + after + payload[after..].find(r).unwrap_or(0);
                builder.row = Some((r.to_string(), start..start + r.len()));
            }
            (Some(_), r) => self.diagnostics.push(
                Diagnostic::error(
                    codes::QUERY_HEADER_INVALID,
                    match r {
                        Some(r) => format!("`-> {r}`: invalid query name"),
                        None => "`->` without a query name".to_string(),
                    },
                )
                .with_span(span(range.clone()))
                .with_hint("`-> other_query`: rows have the shape of `other_query`'s rows"),
            ),
            (None, _) => {}
        }
        if extra.is_some() {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::QUERY_HEADER_INVALID,
                    "unexpected text at the end of the header",
                )
                .with_span(span(range))
                .with_hint("form: `-- name: name [:cardinality] [-> reference_query]`"),
            );
        }

        self.current = Some(builder);
    }

    fn outside(&mut self, range: Range<usize>) {
        if self.outside_reported {
            return;
        }
        self.outside_reported = true;
        self.diagnostics.push(
            Diagnostic::error(codes::QUERY_WITHOUT_NAME, "SQL outside a named query")
                .with_span(span(range))
                .with_hint("each query starts with a `-- name: query_name` header"),
        );
    }

    fn finish_current(&mut self) {
        let Some(b) = self.current.take() else {
            return;
        };
        let Some((name, name_range)) = b.name else {
            return;
        };
        let Some(body) = b.body else {
            self.diagnostics.push(
                Diagnostic::error(codes::QUERY_EMPTY, format!("query `{name}` has no SQL"))
                    .with_span(span(b.header_range)),
            );
            return;
        };

        let sql = self.source[body.clone()].to_string();
        let expansion = self
            .macros
            .filter(|_| sql.contains('@'))
            .map(|m| pgzest_expand::expand_source_at(&sql, body.start, m))
            .filter(|e| !e.is_passthrough() || !e.diagnostics.is_empty());
        let (expanded, placeholders) = match expansion {
            None => (None, b.placeholders),
            Some(e) => {
                let failed = !e.diagnostics.is_empty();
                self.diagnostics.extend(e.diagnostics);
                if failed {
                    return;
                }
                // Parameters of the sent SQL: a fragment may bring some.
                let Some(placeholders) = placeholders_of(&e.sql, body.start) else {
                    return;
                };
                (
                    Some(ExpandedSql {
                        sql: e.sql,
                        map: e.map,
                    }),
                    placeholders,
                )
            }
        };
        // `$name?`: optional parameter; the (attached) `?` is part of the
        // occurrence, removed from the sent SQL.
        let text_of = |r: &Range<usize>| -> Option<u8> {
            match &expanded {
                None => self.source.as_bytes().get(r.end).copied(),
                Some(e) => e.sql.as_bytes().get(r.end - body.start).copied(),
            }
        };
        let placeholders: Vec<(String, Range<usize>, bool)> = placeholders
            .into_iter()
            .map(|(t, r)| {
                if text_of(&r) == Some(b'?') {
                    let end = r.end + 1;
                    (t, r.start..end, true)
                } else {
                    (t, r, false)
                }
            })
            .collect();
        let to_file = |r: Range<usize>| match &expanded {
            None => r,
            Some(e) => e
                .map
                .remap(r.start - body.start, r.end - body.start)
                .map_or(body.clone(), |(s, t)| s..t),
        };
        let Some(params) = self.params(&name, &placeholders, &to_file) else {
            return;
        };
        let text = expanded.as_ref().map_or(sql.as_str(), |e| e.sql.as_str());
        let dynamic = match crate::dynamic::structure(text) {
            Ok(s) if s.is_empty() => None,
            Ok(s) => match dynamic_conditions(&name, &s, &params, body.start) {
                Ok(conditions) => Some(Dynamic {
                    structure: s,
                    conditions,
                }),
                Err((r, msg)) => {
                    self.diagnostics.push(
                        Diagnostic::error(codes::QUERY_HEADER_INVALID, msg)
                            .with_span(span(to_file(r.start + body.start..r.end + body.start))),
                    );
                    return;
                }
            },
            Err((r, msg)) => {
                self.diagnostics.push(
                    Diagnostic::error(codes::QUERY_HEADER_INVALID, msg)
                        .with_span(span(to_file(r.start + body.start..r.end + body.start))),
                );
                return;
            }
        };
        self.queries.push(NamedQuery {
            name,
            name_range,
            cardinality: b.cardinality,
            row: b.row,
            doc: b.doc,
            sql,
            sql_range: body,
            expanded,
            dynamic,
            params,
        });
    }

    /// Classifies the parameters of a query. `None` if their use is invalid
    /// (diagnostics emitted): a query whose parameters are not safe is not
    /// returned, so that no code is generated from it.
    fn params(
        &mut self,
        query: &str,
        placeholders: &[(String, Range<usize>, bool)],
        to_file: &dyn Fn(Range<usize>) -> Range<usize>,
    ) -> Option<Params> {
        let mut positional: Vec<(u32, Range<usize>)> = Vec::new();
        let mut named: Vec<NamedParam> = Vec::new();
        let mut valid = true;

        for (text, range, optional) in placeholders {
            let raw = &text[1..];
            if *optional && raw.bytes().all(|b| b.is_ascii_digit()) {
                valid = false;
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::QUERY_PARAMETER_INVALID,
                        format!("optional positional parameter: `{text}?`"),
                    )
                    .with_span(span(to_file(range.clone())))
                    .with_hint("only a named parameter can be optional (`$status?`); for the jsonb `?` operator, surround it with spaces"),
                );
                continue;
            }
            if !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_digit()) {
                match raw.parse::<u32>() {
                    Ok(n) if n >= 1 => positional.push((n, range.clone())),
                    _ => {
                        valid = false;
                        self.diagnostics.push(
                            Diagnostic::error(
                                codes::QUERY_PARAMETER_INVALID,
                                format!("invalid positional parameter: `{text}`"),
                            )
                            .with_span(span(to_file(range.clone())))
                            .with_hint("positional parameters start at $1"),
                        );
                    }
                }
            } else if is_snake_ident(raw) {
                match named.iter_mut().find(|p| p.name == raw) {
                    Some(p) => {
                        p.occurrences.push(range.clone());
                        p.optional |= *optional;
                    }
                    None => named.push(NamedParam {
                        name: raw.to_string(),
                        occurrences: vec![range.clone()],
                        optional: *optional,
                    }),
                }
            } else {
                valid = false;
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::QUERY_PARAMETER_INVALID,
                        format!("invalid parameter name: `{text}`"),
                    )
                    .with_span(span(to_file(range.clone())))
                    .with_hint("ASCII snake_case: `$page_id`, not `$pageId`"),
                );
            }
        }

        if !positional.is_empty() && !named.is_empty() {
            let (_, first_positional) = &positional[0];
            self.diagnostics.push(
                Diagnostic::error(
                    codes::QUERY_MIXED_PARAMETERS,
                    format!("query `{query}` mixes named and positional parameters"),
                )
                .with_span(span(to_file(first_positional.clone())))
                .with_hint("use only `$name` (recommended) or only `$1 … $n`"),
            );
            return None;
        }

        if !valid {
            return None;
        }

        if !positional.is_empty() {
            let max = positional.iter().map(|(n, _)| *n).max().unwrap_or(0);
            let missing: Vec<String> = (1..=max)
                .filter(|n| !positional.iter().any(|(p, _)| p == n))
                .map(|n| format!("${n}"))
                .collect();
            if !missing.is_empty() {
                let last = positional
                    .iter()
                    .find(|(n, _)| *n == max)
                    .map(|(_, r)| r.clone())
                    .unwrap_or(0..0);
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::QUERY_PARAMETER_GAP,
                        format!("query `{query}` uses ${max} without {}", missing.join(", ")),
                    )
                    .with_span(span(to_file(last))),
                );
                return None;
            }
            return Some(Params::Positional { count: max });
        }

        if named.is_empty() {
            Some(Params::None)
        } else {
            Some(Params::Named(named))
        }
    }
}

/// Duplicate names in the same file: the second occurrence is reported and
/// removed, so that no generation picks one arbitrarily.
fn report_duplicates(queries: &mut Vec<NamedQuery>, diagnostics: &mut Vec<Diagnostic>) {
    let mut seen: HashMap<String, Range<usize>> = HashMap::new();
    queries.retain(|q| {
        if seen.contains_key(&q.name) {
            diagnostics.push(
                Diagnostic::error(
                    codes::QUERY_DUPLICATE_NAME,
                    format!("query `{}` already defined in this file", q.name),
                )
                .with_span(span(q.name_range.clone())),
            );
            false
        } else {
            seen.insert(q.name.clone(), q.name_range.clone());
            true
        }
    });
}

/// Parameter of each block: exactly one per block, used only inside blocks,
/// without a `?` marker; a bounded number of variants.
fn dynamic_conditions(
    query: &str,
    s: &crate::dynamic::Structure,
    params: &Params,
    base: usize,
) -> Result<Vec<String>, (Range<usize>, String)> {
    let named: &[NamedParam] = match params {
        Params::Named(p) => p,
        Params::None => &[],
        Params::Positional { .. } => {
            let r = s
                .blocks
                .first()
                .map_or_else(|| s.choices[0].range.clone(), |b| b.range.clone());
            return Err((
                r,
                "query with variants: named parameters only (`$name`)".into(),
            ));
        }
    };
    let in_block = |r: &Range<usize>| s.blocks.iter().any(|b| b.inner.contains(&(r.start - base)));
    let mut conditions = Vec::new();
    for b in &s.blocks {
        let inside: Vec<&NamedParam> = named
            .iter()
            .filter(|p| {
                p.occurrences
                    .iter()
                    .any(|r| b.inner.contains(&(r.start - base)))
            })
            .collect();
        match inside.as_slice() {
            [p] => {
                if p.occurrences.iter().any(|r| !in_block(r)) {
                    return Err((
                        b.range.clone(),
                        format!(
                            "`${}` decides this block: it cannot be used outside blocks",
                            p.name
                        ),
                    ));
                }
                if p.optional {
                    return Err((
                        b.range.clone(),
                        format!(
                            "`${}?`: a block parameter is already optional, drop the `?`",
                            p.name
                        ),
                    ));
                }
                conditions.push(p.name.clone());
            }
            [] => {
                return Err((
                    b.range.clone(),
                    "block without a parameter: nothing decides whether it is included".into(),
                ));
            }
            _ => {
                return Err((
                    b.range.clone(),
                    "a block depends on a single parameter: write one block per parameter".into(),
                ));
            }
        }
    }
    let n = crate::dynamic::variant_count(s);
    if n > crate::dynamic::MAX_VARIANTS {
        let r = s
            .blocks
            .first()
            .map_or_else(|| s.choices[0].range.clone(), |b| b.range.clone());
        return Err((
            r,
            format!(
                "`{query}`: {n} variants, at most {} (each one is checked)",
                crate::dynamic::MAX_VARIANTS
            ),
        ));
    }
    Ok(conditions)
}

// ── Utilities ─────────────────────────────────────────────────────────────────

/// Reference variant: all blocks, the first option of each choice.
pub fn reference_selection(s: &crate::dynamic::Structure) -> crate::dynamic::Selection {
    crate::dynamic::Selection {
        blocks: vec![true; s.blocks.len()],
        choices: vec![0; s.choices.len()],
    }
}

/// `sql` where each `$name?` marker (optional parameter, attached `?`) has
/// its `?` replaced by a space: readable by the parser, positions unchanged.
/// Strings and comments are not touched.
pub fn blank_optional_markers(sql: &str) -> String {
    let index = LineIndex::new(sql);
    let Ok(tokens) = Tokenizer::new(&PostgreSqlDialect {}, sql)
        .with_unescape(false)
        .tokenize_with_location()
    else {
        return sql.to_string();
    };
    let mut bytes = sql.as_bytes().to_vec();
    for pair in tokens.windows(2) {
        if let Token::Placeholder(p) = &pair[0].token
            && p.starts_with('$')
            && let Some(start) = index.offset(pair[0].span.start)
            && bytes.get(start + p.len()) == Some(&b'?')
            && index.offset(pair[1].span.start) == Some(start + p.len())
        {
            bytes[start + p.len()] = b' ';
        }
    }
    let out = String::from_utf8(bytes).unwrap_or_else(|_| sql.to_string());
    // Blocks and choices: the reference variant, same length.
    match crate::dynamic::structure(&out) {
        Ok(s) if !s.is_empty() => crate::dynamic::render(&out, &s, &reference_selection(&s)),
        _ => out,
    }
}

/// `$…` of `sql` (SQL after fragment insertion), positions shifted by
/// `base`. `None` if the tokenizer cannot read it (parsing will report it).
fn placeholders_of(sql: &str, base: usize) -> Option<Vec<(String, Range<usize>)>> {
    let index = LineIndex::new(sql);
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, sql)
        .with_unescape(false)
        .tokenize_with_location()
        .ok()?;
    Some(
        tokens
            .iter()
            .filter_map(|t| match &t.token {
                Token::Placeholder(p) if p.starts_with('$') => {
                    let start = index.offset(t.span.start)?;
                    Some((p.clone(), base + start..base + start + p.len()))
                }
                _ => None,
            })
            .collect(),
    )
}

/// Content of a header if `comment` (text after `--`) is one.
fn header_payload(comment: &str) -> Option<&str> {
    comment.trim_start().strip_prefix("name:").map(str::trim)
}

/// Documentation line: the comment text without the space following `--`
/// nor the line break.
fn doc_line(comment: &str) -> String {
    let text = comment.trim_end_matches(['\n', '\r']);
    text.strip_prefix(' ').unwrap_or(text).to_string()
}

/// `[a-z_][a-z0-9_]*`: usable as is as a Rust identifier, and losslessly
/// convertible to camelCase for TypeScript.
fn is_snake_ident(s: &str) -> bool {
    let mut bytes = s.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z' | b'_'))
        && bytes.all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

fn span(range: Range<usize>) -> Span {
    Span {
        start: range.start,
        end: range.end,
    }
}
