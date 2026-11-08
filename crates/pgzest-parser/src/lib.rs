mod convert;
pub mod dialect;
pub mod error;
mod grammar;
mod ids;
pub mod normalize;
mod pipeline;
pub mod plpgsql;
mod split;

pub(crate) use ids::next_node_id;

pub use error::ParseError;
pub use normalize::{NormalizedSelect, normalize_select};

use pgzest_ast::Statement;

/// Reason why a statement did not end up in the AST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// sqlparser-rs could not parse the statement. At this level we can NOT
    /// distinguish "valid PostgreSQL we do not support" (GRANT,
    /// CREATE EXTENSION, …) from a real syntax error. It is up to the consumer
    /// to decide what to do with it — see the `[parse] unparsable` config.
    Unparsable(String),
    /// Parsed by sqlparser but deliberately not modeled in our AST.
    /// Here we know the SQL is valid, just out of scope.
    Unsupported(String),
    /// The statement is in the AST, but the **body** of a PL/pgSQL function or
    /// procedure could not be parsed.
    ///
    /// Practical consequence: this body falls back to an almost blind textual
    /// fallback. Variables, types, `RETURN`s and trigger context are no
    /// longer checked. A single unhandled construct is enough — hence
    /// the importance of saying so instead of implying a complete analysis.
    UnparsedBody { name: String, reason: String },
}

/// A statement skipped during the parse, with its position in the source.
#[derive(Debug, Clone)]
pub struct SkippedStatement {
    /// Text of the statement (without the final `;`).
    pub sql: String,
    /// Byte offset of the statement's start in the full source.
    pub start: usize,
    /// End byte offset (exclusive).
    pub end: usize,
    pub reason: SkipReason,
}

/// Full result of a parse: what was understood, and what was skipped.
#[derive(Debug, Clone, Default)]
pub struct ParseOutcome {
    pub statements: Vec<Statement>,
    pub skipped: Vec<SkippedStatement>,
}

pub use split::SplitStatement;

/// Splits `text` into statements without parsing them: the ranges the
/// parser itself works on (strings, dollar bodies and comments are never
/// cut). The second value is the unreadable region ending the text
/// (unterminated string…), with the tokenizer's message.
pub fn split_statements(
    text: &str,
) -> (
    Vec<SplitStatement>,
    Option<(std::ops::Range<usize>, String)>,
) {
    split::split_statements(text)
}

/// Parses `sql` and returns only the understood statements.
///
/// Unparsable or unsupported statements are silently skipped.
/// Prefer [`parse_with_outcome`] to know what was skipped — otherwise
/// an invalid file is indistinguishable from a clean one.
pub fn parse(sql: &str) -> Result<Vec<Statement>, ParseError> {
    Ok(parse_with_outcome(sql)?.statements)
}

/// Parses `sql`, keeping track of skipped statements.
pub fn parse_with_outcome(sql: &str) -> Result<ParseOutcome, ParseError> {
    parse_at_with_outcome(sql, 0)
}

/// How to translate an offset of the submitted text into the document's frame.
///
/// The parser assigns spans **already translated**, rather than letting a
/// consumer translate them afterwards. Consequence: everything that reads the AST
/// — checker, linter, hover, go-to-definition, codegen — gets correct
/// positions, not only diagnostics.
/// Owned rather than borrowed: the mapping must go through a
/// thread-local (expression spans) and the PL/pgSQL parser, which cannot
/// carry a lifetime. The clone is a `Vec` of segments, negligible
/// compared to a parse.
#[derive(Clone)]
pub enum Offsets {
    /// Constant offset: the submitted text is a contiguous fragment of the document.
    Shift(usize),
    /// Arbitrary mapping: the submitted text was generated from the
    /// document (macro expansion, preprocessing).
    Mapped(pgzest_ast::SourceMap),
}

impl Offsets {
    /// Translates a single offset.
    pub(crate) fn point(&self, offset: usize) -> usize {
        match self {
            Self::Shift(base) => base + offset,
            // A point without an antecedent falls back to the nearest copied
            // fragment: an approximate position is better than none.
            Self::Mapped(map) => map.nearest_source(offset).unwrap_or(offset),
        }
    }

    /// Translates a range. The end is bounded to the start's segment, otherwise a range
    /// overlapping generated text would extend absurdly into the source.
    pub(crate) fn span(&self, start: usize, end: usize) -> (usize, usize) {
        match self {
            Self::Shift(base) => (base + start, base + end),
            Self::Mapped(map) => map
                .remap(start, end)
                .unwrap_or_else(|| (self.point(start), self.point(end))),
        }
    }

    /// Translates a range enclosing whole constructs (a statement): its end
    /// follows the source across generated text, see
    /// [`pgzest_ast::SourceMap::remap_enclosing`].
    pub(crate) fn extent(&self, start: usize, end: usize) -> (usize, usize) {
        match self {
            Self::Shift(base) => (base + start, base + end),
            Self::Mapped(map) => map
                .remap_enclosing(start, end)
                .unwrap_or_else(|| (self.point(start), self.point(end))),
        }
    }
}

/// Parses a SQL fragment extracted from a larger document.
///
/// `base_offset` is the byte offset of the start of `sql` in the enclosing
/// document. All produced spans — statements, skipped statements, PL/pgSQL
/// bodies, expressions — are expressed in the document's frame.
///
/// This is what makes it possible to analyze SQL that does not live in a `.sql` file:
/// a template literal in a TypeScript file, a block in a Markdown file.
///
/// [`parse`] and [`parse_with_outcome`] are the `base_offset = 0` case.
pub fn parse_at(sql: &str, base_offset: usize) -> Result<Vec<Statement>, ParseError> {
    Ok(parse_with_offsets(sql, Offsets::Shift(base_offset))?.statements)
}

/// Like [`parse_at`], keeping track of skipped statements.
pub fn parse_at_with_outcome(sql: &str, base_offset: usize) -> Result<ParseOutcome, ParseError> {
    parse_with_offsets(sql, Offsets::Shift(base_offset))
}

/// Parses SQL **generated** from another text, with spans mapped back to the source.
///
/// `map` describes the origin of each fragment of `sql`. The AST's spans
/// therefore directly designate the edited file — a `.pgsql` for example — and
/// no consumer has any mapping back to do.
pub fn parse_mapped(sql: &str, map: &pgzest_ast::SourceMap) -> Result<Vec<Statement>, ParseError> {
    Ok(parse_with_offsets(sql, Offsets::Mapped(map.clone()))?.statements)
}

/// Like [`parse_mapped`], keeping track of skipped statements.
pub fn parse_mapped_with_outcome(
    sql: &str,
    map: &pgzest_ast::SourceMap,
) -> Result<ParseOutcome, ParseError> {
    parse_with_offsets(sql, Offsets::Mapped(map.clone()))
}

fn parse_with_offsets(sql: &str, offsets: Offsets) -> Result<ParseOutcome, ParseError> {
    pipeline::parse_text(sql, 0, &offsets)
}

pub(crate) fn set_stmt_span(stmt: &mut Statement, start: usize, end: usize) {
    use pgzest_ast::{DdlStatement, Statement::*};
    let span = pgzest_ast::node::Span {
        start: start as u32,
        end: end as u32,
    };
    match stmt {
        Select(s) => s.span = span,
        Insert(s) => s.span = span,
        Update(s) => s.span = span,
        Delete(s) => s.span = span,
        Utility(u) => u.span = span,
        Ddl(ddl) => match ddl {
            DdlStatement::CreateFunction(cf) => cf.span = span,
            DdlStatement::CreateProcedure(cp) => cp.span = span,
            DdlStatement::CreatePolicy(p) => p.span = span,
            DdlStatement::CreateTable(ct) => ct.span = span,
            DdlStatement::CreateIndex(ci) => ci.span = span,
            _ => {}
        },
    }
}

/// Expose single-statement conversion for testing purposes.
/// Takes a pre-parsed sqlparser statement and converts it to a pgzest_ast Statement.
pub fn convert_statement_pub(stmt: sqlparser::ast::Statement) -> Result<Statement, ParseError> {
    let _ids = ids::session();
    convert::convert_statement(stmt)
}

#[cfg(test)]
mod skip_tests {
    use super::*;

    #[test]
    fn invalid_sql_is_reported_not_swallowed() {
        let out = parse_with_outcome("SELCT * FRM;").expect("parse must not fail");
        assert!(out.statements.is_empty());
        assert_eq!(out.skipped.len(), 1, "one skipped statement expected");
        assert!(matches!(out.skipped[0].reason, SkipReason::Unparsable(_)));
    }

    #[test]
    fn the_skipped_statement_carries_its_position() {
        let sql = "CREATE TABLE t (a INT);\nSELCT oops;";
        let out = parse_with_outcome(sql).unwrap();
        assert_eq!(out.statements.len(), 1, "the CREATE TABLE must pass");
        assert_eq!(out.skipped.len(), 1);
        let sk = &out.skipped[0];
        // The span must point at the 2nd line, not at the start of the file.
        assert!(
            sk.start >= 24,
            "start={} should be on the 2nd line",
            sk.start
        );
        assert!(sql[sk.start..sk.end].contains("SELCT"));
    }

    #[test]
    fn valid_sql_produces_no_skips() {
        let out = parse_with_outcome("CREATE TABLE t (a INT); SELECT a FROM t;").unwrap();
        assert_eq!(out.statements.len(), 2);
        assert!(
            out.skipped.is_empty(),
            "unexpected skips: {:?}",
            out.skipped
        );
    }

    #[test]
    fn an_invalid_statement_does_not_kill_the_following_ones() {
        let sql = "SELCT bad; CREATE TABLE t (a INT); SELECT a FROM t;";
        let out = parse_with_outcome(sql).unwrap();
        assert_eq!(out.statements.len(), 2);
        assert_eq!(out.skipped.len(), 1);
    }
}

#[cfg(test)]
mod parse_at_tests {
    use super::*;

    /// Span of a statement, whatever its kind.
    fn span_of(stmt: &Statement) -> (u32, u32) {
        use pgzest_ast::{DdlStatement, Statement::*};
        let sp = match stmt {
            Select(s) => &s.span,
            Insert(s) => &s.span,
            Update(s) => &s.span,
            Delete(s) => &s.span,
            Utility(u) => &u.span,
            Ddl(d) => match d {
                DdlStatement::CreateFunction(c) => &c.span,
                DdlStatement::CreateProcedure(c) => &c.span,
                DdlStatement::CreatePolicy(c) => &c.span,
                DdlStatement::CreateTable(c) => &c.span,
                _ => return (0, 0),
            },
        };
        (sp.start, sp.end)
    }

    #[test]
    fn base_offset_zero_equals_parse() {
        let sql = "SELECT 1;";
        let a = parse_at(sql, 0).unwrap();
        let b = parse(sql).unwrap();
        assert_eq!(span_of(&a[0]), span_of(&b[0]));
    }

    #[test]
    fn spans_are_shifted_by_base_offset() {
        let fragment = "SELECT 1;";
        let base = 100;
        let at_zero = parse_at(fragment, 0).unwrap();
        let at_base = parse_at(fragment, base).unwrap();

        let (s0, e0) = span_of(&at_zero[0]);
        let (sb, eb) = span_of(&at_base[0]);
        assert_eq!(sb, s0 + base as u32, "start shifted");
        assert_eq!(eb, e0 + base as u32, "end shifted");
    }

    #[test]
    fn the_span_points_into_the_enclosing_document() {
        // Simulates SQL extracted from a TypeScript file.
        let doc = "const q = sql`\nSELECT id FROM users;\n`;\n";
        let start = doc.find("SELECT").unwrap();
        let fragment = "SELECT id FROM users;";

        let stmts = parse_at(fragment, start).unwrap();
        let (s, e) = span_of(&stmts[0]);

        // The span must allow finding the text in the full document.
        assert_eq!(
            &doc[s as usize..e as usize],
            "SELECT id FROM users",
            "the span must index the document, not the fragment"
        );
    }

    #[test]
    fn skipped_statements_are_shifted_too() {
        let doc = "// some TS\nSELCT oops;";
        let start = doc.find("SELCT").unwrap();
        let out = parse_at_with_outcome("SELCT oops;", start).unwrap();
        assert_eq!(out.skipped.len(), 1);
        let sk = &out.skipped[0];
        assert_eq!(
            &doc[sk.start..sk.end],
            "SELCT oops",
            "the skip must point into the enclosing document"
        );
    }

    #[test]
    fn several_statements_keep_their_relative_positions() {
        let fragment = "SELECT 1;\nSELECT 2;";
        let base = 50;
        let stmts = parse_at(fragment, base).unwrap();
        assert_eq!(stmts.len(), 2);
        let (s1, _) = span_of(&stmts[0]);
        let (s2, _) = span_of(&stmts[1]);
        assert_eq!(s1, base as u32);
        assert_eq!(s2, base as u32 + 10, "2nd statement after the \\n");
    }
}

#[cfg(test)]
mod utf8_span_tests {
    use super::*;

    fn stmt_span(stmt: &Statement) -> (usize, usize) {
        use pgzest_ast::{DdlStatement, Statement::*};
        let sp = match stmt {
            Select(s) => &s.span,
            Insert(s) => &s.span,
            Update(s) => &s.span,
            Delete(s) => &s.span,
            Utility(u) => &u.span,
            Ddl(d) => match d {
                DdlStatement::CreateTable(c) => &c.span,
                DdlStatement::CreateFunction(c) => &c.span,
                DdlStatement::CreateProcedure(c) => &c.span,
                DdlStatement::CreatePolicy(c) => &c.span,
                _ => return (0, 0),
            },
        };
        (sp.start as usize, sp.end as usize)
    }

    /// Regression: `split_statements` did `bytes[i] as char`, reinterpreting
    /// each UTF-8 byte as a Latin-1 code point. `→` (3 bytes) became
    /// 3 characters of 2 bytes, inflating the length by 3 and shifting the spans
    /// of all following statements. A single accented character in a comment
    /// was enough to make diagnostics point to the wrong place.
    #[test]
    fn an_accent_does_not_shift_following_spans() {
        let sql = "-- naïve → über\nSELECT nope FROM t;";
        let stmts = parse(sql).unwrap();
        assert_eq!(stmts.len(), 1);
        let (start, _) = stmt_span(&stmts[0]);
        assert_eq!(
            &sql[start..start + 6],
            "SELECT",
            "the span must start on SELECT, not drift into the word"
        );
    }

    #[test]
    fn non_ascii_text_is_not_corrupted() {
        // A literal with accented characters must go through the parse intact.
        let sql = "SELECT 'naïve Zürich' AS x;";
        let stmts = parse(sql).unwrap();
        assert_eq!(stmts.len(), 1);
        let (start, end) = stmt_span(&stmts[0]);
        assert_eq!(
            &sql[start..end],
            "SELECT 'naïve Zürich' AS x",
            "the text must be identical to the source"
        );
    }

    #[test]
    fn several_statements_after_non_ascii() {
        let sql = "-- “quoted” ñ header\nSELECT 1;\n-- second — em dash\nSELECT 2;";
        let stmts = parse(sql).unwrap();
        assert_eq!(stmts.len(), 2);
        for st in &stmts {
            let (start, _) = stmt_span(st);
            assert_eq!(
                &sql[start..start + 6],
                "SELECT",
                "each span must start on SELECT"
            );
        }
    }

    /// The span starts on the SQL, not on the comments preceding it.
    /// `split_statements` splits on `;`, so comments stay attached
    /// to the following statement — without this skip, a directive placed just before did
    /// not cover the statement it targeted.
    #[test]
    fn the_span_skips_leading_comments() {
        let sql = "CREATE TABLE t (a INT);\n-- a comment\n-- and another\nSELECT a FROM t;";
        let stmts = parse(sql).unwrap();
        assert_eq!(stmts.len(), 2);
        let (start, _) = stmt_span(&stmts[1]);
        assert_eq!(&sql[start..start + 6], "SELECT");
    }

    #[test]
    fn the_span_also_skips_block_comments() {
        let sql = "CREATE TABLE t (a INT);\n/* block\n   multiline */\nSELECT a FROM t;";
        let stmts = parse(sql).unwrap();
        let (start, _) = stmt_span(&stmts[1]);
        assert_eq!(&sql[start..start + 6], "SELECT");
    }

    #[test]
    fn a_dollar_quote_with_accents_stays_intact() {
        let sql = "CREATE FUNCTION f() RETURNS TEXT LANGUAGE sql AS $$ SELECT 'ñü' $$;";
        let stmts = parse(sql).unwrap();
        assert_eq!(stmts.len(), 1);
        let (start, end) = stmt_span(&stmts[0]);
        assert!(
            sql[start..end].contains("'ñü'"),
            "the dollar-quoted body must stay intact, got: {:?}",
            &sql[start..end]
        );
    }
}

/// Last segment of an `ObjectName`, to name a function in a message.
pub(crate) fn object_name_last(name: &pgzest_ast::expr::ObjectName) -> String {
    name.0
        .last()
        .map(|i| i.value.clone())
        .unwrap_or_else(|| "?".to_string())
}
