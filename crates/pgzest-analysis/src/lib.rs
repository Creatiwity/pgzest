//! The analysis pipeline shared by the CLI and the language server:
//! parse → check → lint.
//!
//! - [`check`]: statement checking — the semantic analysis of
//!   `pgzest-sema`, plus the rules contributed by plugins ([`rule::CheckRule`],
//!   run by [`Checker`]), which see the schema through `SchemaProvider`;
//! - [`project`]: the incremental project database (salsa) the language
//!   server runs on;
//! - the functions at the root chain the passes. Checking and `pgzest-lint`
//!   are independent passes that do not know about each other: anything
//!   that chains them lives here, above them.

pub mod check;
pub mod checker;
pub mod project;
pub mod rule;

pub use check::{check_script, check_sql, check_sql_self_contained, check_statements};
pub use checker::Checker;

use pgzest_catalog::ProjectCatalog;
use pgzest_diagnostics::{Diagnostic, Severity, Span, Suppressions, codes};
use pgzest_parser::{LostSyntax, SkipReason, SkippedStatement};
use pgzest_schema::{ParseConfig, SkipLevel};

// ── Parser tolerance ──────────────────────────────────────────────────────────

/// Turns the statements skipped by the parser into diagnostics, per the config.
///
/// This is where the "swallow or report" choice is made — no longer in the
/// parser, which now merely reports what it skipped.
pub fn skipped_to_diagnostics(skipped: &[SkippedStatement], cfg: &ParseConfig) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    for sk in skipped {
        let (level, code, label) = match &sk.reason {
            SkipReason::Unparsable(why) => (
                cfg.unparsable,
                codes::UNPARSABLE_STATEMENT,
                format!("unreadable statement, ignored by the analysis: {why}"),
            ),
            SkipReason::Unsupported(what) => (
                cfg.unsupported,
                codes::UNSUPPORTED_STATEMENT,
                format!("statement not modeled, ignored by the analysis: {what}"),
            ),
            // The statement itself is analyzed, but not the function body.
            // Same level as `unparsable`: in both cases there is SQL we could
            // not read.
            SkipReason::UnparsedBody { name, reason } => (
                cfg.unparsable,
                codes::UNPARSED_FUNCTION_BODY,
                format!(
                    "body of '{name}' not analyzed: {reason}. \
                     Variables, types, RETURN and trigger context are not checked"
                ),
            ),
        };

        let severity = match level {
            SkipLevel::Ignore => continue,
            SkipLevel::Warn => Severity::Warning,
            SkipLevel::Error => Severity::Error,
        };

        let mut d = Diagnostic::error(code, label);
        d.severity = severity;
        out.push(
            d.with_span(Span {
                start: sk.start,
                end: sk.end,
            })
            .with_hint(hint_for(&sk.reason)),
        );
    }

    out
}

/// Where a gap of the AST is reported, prefilled from the issue form
/// `.github/ISSUE_TEMPLATE/syntax-not-modeled.yml`.
const ISSUE_URL: &str = "https://github.com/Creatiwity/pgzest/issues/new";

/// Percent-encoding of a URL query value.
fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Statements read with syntax the AST does not hold → `syntax-not-modeled`
/// warnings, each with the link to a prefilled issue. The link carries the
/// lost words and the kind of statement, never the statement itself: the
/// project's SQL stays private, the reporter writes a minimal example.
pub fn lost_to_diagnostics(lost: &[LostSyntax]) -> Vec<Diagnostic> {
    lost.iter()
        .map(|l| {
            let words = l.words.join(" ");
            let title = format!("Syntax not modeled: {words} in {}", l.statement);
            let url = format!(
                "{ISSUE_URL}?template=syntax-not-modeled.yml&title={}&syntax={}&statement={}&version={}",
                url_encode(&title),
                url_encode(&words),
                url_encode(&l.statement),
                url_encode(env!("CARGO_PKG_VERSION")),
            );
            let mut d = Diagnostic::error(
                codes::SYNTAX_NOT_MODELED,
                format!(
                    "pgzest reads this statement but does not model `{words}`: the analysis \
                     ignores it, and SQL generated from the model (migrations) may lack it"
                ),
            );
            d.severity = Severity::Warning;
            d.with_span(Span {
                start: l.start,
                end: l.end,
            })
            .with_hint(format!(
                "a gap of pgzest, not of your SQL: report it ({url}), with a minimal \
                 statement using it; `[rules] \"syntax-not-modeled\" = \"off\"` silences it"
            ))
        })
        .collect()
}

fn hint_for(reason: &SkipReason) -> &'static str {
    match reason {
        SkipReason::UnparsedBody { .. } => {
            "a single unhandled PL/pgSQL construct is enough to disable the \
             analysis of the whole body; `-- pgzest-disable-next-line \
             unparsed-function-body` to silence it here"
        }
        SkipReason::Unparsable(_) => {
            "if this SQL is valid, it is a parser gap: \
             `-- pgzest-disable-next-line unparsable-statement` to silence it here, \
             or `[parse] unparsable = \"ignore\"` for the whole project"
        }
        SkipReason::Unsupported(_) => {
            "valid statement but outside the analysis scope; \
             `[parse] unsupported = \"ignore\"` to stop reporting it"
        }
    }
}

// ── Named queries ─────────────────────────────────────────────────────────────

/// Format diagnostics of a `[queries] directory` file
/// (`site/src/content/docs/reference/named-queries.md`). They add to the usual analysis of the file,
/// which already checks each statement against the schema.
///
/// Shared by the CLI and the LSP: a query file must be judged the same way
/// in the editor and in CI.
pub fn query_file_diagnostics(
    path: &std::path::Path,
    sql: &str,
    macros: &pgzest_expand::MacroSet,
) -> Vec<Diagnostic> {
    let is_sql = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("sql"));
    if !is_sql {
        return vec![
            Diagnostic::error(
                codes::QUERY_FILE_EXTENSION,
                "a named query file is a `.sql` file",
            )
            .with_hint(
                "a named query uses the project's fragments (`@name`), not \
                 a .pgsql file",
            ),
        ];
    }
    // Macro errors are those of the file's desugaring, already reported
    // with it.
    pgzest_query::parse_query_file_with(sql, macros)
        .diagnostics
        .into_iter()
        .filter(|d| d.code != pgzest_expand::expand::MACRO_ERROR)
        .collect()
}

/// What prevents describing the named queries of a file (PLAN.md §4):
/// a parameter nothing types, a contradicted declared cardinality, a
/// statement outside the descriptor's scope. Analysis errors themselves are
/// reported by the file check, not here.
///
/// `here`: module of the file in its query directory; `others`: the queries
/// of that directory, to check a query that reuses the rows of another
/// (`-> other`, `-> a::b::other`).
pub fn query_descriptor_diagnostics(
    sql: &str,
    catalog: &ProjectCatalog,
    macros: &pgzest_expand::MacroSet,
    here: &[String],
    others: &[pgzest_query::QueryModule],
) -> Vec<Diagnostic> {
    let own = [
        codes::QUERY_PARAMETER_UNTYPED,
        codes::QUERY_CARDINALITY_MISMATCH,
        codes::QUERY_NOT_DESCRIBABLE,
    ];
    let file = pgzest_query::parse_query_file_with(sql, macros);
    let mut out = Vec::new();
    let mut described = Vec::new();
    for q in &file.queries {
        match pgzest_query::describe_named(catalog, q) {
            Ok(d) => described.push((q, d)),
            Err(ds) => out.extend(ds.into_iter().filter(|d| own.contains(&d.code))),
        }
    }
    // This file is authoritative over the saved version of its module.
    let mut modules: Vec<pgzest_query::QueryModule> =
        others.iter().filter(|m| m.path != here).cloned().collect();
    modules.push(pgzest_query::QueryModule {
        source: None,
        path: here.to_vec(),
        descriptors: described.iter().map(|(_, d)| d.clone()).collect(),
    });
    for (q, d) in &described {
        let (Some(r), Some((_, range))) = (&d.row, &q.row) else {
            continue;
        };
        let target = pgzest_query::resolve_row(here, r, &modules).map(|(_, t)| t);
        if let Err(e) = pgzest_query::check_row_reference(d, target) {
            out.push(Diagnostic::error(codes::QUERY_ROW_MISMATCH, e).with_span(
                pgzest_diagnostics::Span {
                    start: range.start,
                    end: range.end,
                },
            ));
        }
    }
    out
}

/// Queries of a query directory, per module (`order/book.sql` →
/// `["order", "book"]`). `read` gives the text of a file (document open in
/// the editor, otherwise disk). An undescribed query is absent: its file
/// reports it.
pub fn query_modules(
    dir: &std::path::Path,
    catalog: &ProjectCatalog,
    macros: &pgzest_expand::MacroSet,
    read: &dyn Fn(&std::path::Path) -> Option<String>,
) -> Vec<pgzest_query::QueryModule> {
    let mut files = Vec::new();
    collect_sql_files(dir, &mut files);
    files.sort();
    files
        .iter()
        .filter_map(|f| {
            let text = read(f)?;
            let rel = f.strip_prefix(dir).ok()?.with_extension("");
            let path = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            let descriptors = pgzest_query::parse_query_file_with(&text, macros)
                .queries
                .iter()
                .filter_map(|q| pgzest_query::describe_named(catalog, q).ok())
                .collect();
            Some(pgzest_query::QueryModule {
                path,
                descriptors,
                source: None,
            })
        })
        .collect()
}

fn collect_sql_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_sql_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "sql") {
            out.push(p);
        }
    }
}

// ── Lint ──────────────────────────────────────────────────────────────────────

/// Parses `sql` then lints it, with comment directives applied.
///
/// This function lives here and not in `pgzest-lint`: the linter
/// deliberately does not depend on the parser, to stay an independent pass.
/// What chains parse and lint therefore belongs to the facade.
///
/// Replaces the former `pgzest_lint::lint_sql`, which was a publicly
/// exported `unimplemented!()` — a guaranteed panic for any caller.
pub fn lint_sql(sql: &str, config: &pgzest_lint::LintConfig) -> Vec<Diagnostic> {
    let Ok(stmts) = pgzest_parser::parse(sql) else {
        return vec![Diagnostic::error(
            codes::PARSE_ERROR,
            "parse error".to_string(),
        )];
    };
    let mut diags = pgzest_lint::lint_statements(&stmts, config).diagnostics;
    Suppressions::scan(sql).apply(&mut diags);
    diags
}

// ── Full analysis ─────────────────────────────────────────────────────────────

/// Options of an analysis.
#[derive(Debug, Clone, Default)]
pub struct AnalysisOptions {
    pub parse: ParseConfig,
}

/// Result of a full analysis.
pub struct AnalysisResult {
    /// Diagnostics, comment directives already applied.
    pub diagnostics: Vec<Diagnostic>,
    /// Statements skipped by the parser, whatever the configured level.
    pub skipped: Vec<SkippedStatement>,
    /// Directives found in the source.
    pub suppressions: Suppressions,
}

impl AnalysisResult {
    /// True if at least one diagnostic has Error severity.
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }
}

/// Full pipeline over a source: parse → tolerance → semantic → check,
/// then application of the comment directives.
///
/// This is the preferred entry point for the CLI, the LSP and the bindings:
/// it guarantees everyone applies the same tolerance policy and the same
/// suppressions.
pub fn analyze_source(
    sql: &str,
    schema: &ProjectCatalog,
    opts: &AnalysisOptions,
) -> AnalysisResult {
    analyze_parsed(sql, Some(schema), opts)
}

/// Checks the statements: against `catalog` if given (the file runs on that
/// schema), otherwise by replaying the file itself, each statement checked
/// as it is applied.
fn checks(stmts: &[pgzest_ast::Statement], catalog: Option<&ProjectCatalog>) -> Vec<Diagnostic> {
    match catalog {
        Some(c) => crate::check_statements(stmts, c),
        None => crate::check_script(stmts),
    }
}

fn analyze_parsed(
    sql: &str,
    catalog: Option<&ProjectCatalog>,
    opts: &AnalysisOptions,
) -> AnalysisResult {
    let suppressions = Suppressions::scan(sql);

    let outcome = match pgzest_parser::parse_with_outcome(sql) {
        Ok(o) => o,
        Err(e) => {
            let mut diagnostics = vec![Diagnostic::error(
                codes::PARSE_ERROR,
                format!("parse error: {e}"),
            )];
            suppressions.apply(&mut diagnostics);
            return AnalysisResult {
                diagnostics,
                skipped: Vec::new(),
                suppressions,
            };
        }
    };

    let mut diagnostics = skipped_to_diagnostics(&outcome.skipped, &opts.parse);
    diagnostics.extend(lost_to_diagnostics(&outcome.lost));
    diagnostics.extend(header_diagnostic(sql));
    diagnostics.extend(checks(&outcome.statements, catalog));
    suppressions.apply(&mut diagnostics);

    AnalysisResult {
        diagnostics,
        skipped: outcome.skipped,
        suppressions,
    }
}

/// Malformed migration header (PLAN.md, D9): a misread header would analyze
/// the file against a context other than the announced one.
fn header_diagnostic(text: &str) -> Option<Diagnostic> {
    let Some(Err(e)) = pgzest_schema::MigrationHeader::find(text) else {
        return None;
    };
    Some(
        Diagnostic::error(codes::MIGRATION_HEADER_INVALID, e.message)
            .with_span(pgzest_diagnostics::Span {
                start: e.span.start,
                end: e.span.end,
            })
            .with_hint("expected form: `-- pgzest: postgres=18 extensions=name@version,…`"),
    )
}

/// Self-contained variant: the schema is derived from the DDL in the source.
pub fn analyze_source_self_contained(sql: &str, opts: &AnalysisOptions) -> AnalysisResult {
    analyze_parsed(sql, None, opts)
}

/// Check diagnostics of `sql` against `schema`.
pub fn analyze_sql(sql: &str, schema: &ProjectCatalog) -> Vec<Diagnostic> {
    match pgzest_parser::parse(sql) {
        Ok(stmts) => crate::check_statements(&stmts, schema),
        Err(e) => vec![Diagnostic::error(
            codes::PARSE_ERROR,
            format!("parse error: {e}"),
        )],
    }
}

/// Check diagnostics of `sql`, the schema derived from the DDL it contains
/// (each statement checked as it is applied).
///
/// Mode used to check a self-contained file, without an external schema.
pub fn analyze_sql_self_contained(sql: &str) -> Vec<Diagnostic> {
    crate::check_sql_self_contained(sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyzes_a_self_contained_file() {
        let sql = "CREATE TABLE t (a INT); SELECT a FROM t;";
        let diags = analyze_sql_self_contained(sql);
        assert!(diags.is_empty(), "no diagnostic expected, got: {diags:?}");
    }

    #[test]
    fn reports_a_missing_column() {
        let sql = "CREATE TABLE t (a INT); SELECT nope FROM t;";
        let diags = analyze_sql_self_contained(sql);
        assert!(
            diags.iter().any(|d| d.code == codes::COLUMN_NOT_FOUND),
            "column-not-found expected, got: {diags:?}"
        );
    }

    /// Regression: invalid SQL must no longer pass silently.
    ///
    /// Historically `pgzest-parser` did `Err(_) => continue` and returned
    /// `Ok(0 statements)` on any broken SQL, making `PARSE_ERROR`
    /// unreachable and `check` falsely reassuring. Skipped statements are
    /// now reported, with a level driven by the config.
    #[test]
    fn invalid_sql_is_reported() {
        let _diags = analyze_sql_self_contained("SELCT * FRM;");
        // analyze_sql_self_contained keeps the historical contract (no config);
        // analyze_source is what applies the tolerance policy.
        let r = analyze_source_self_contained("SELCT * FRM;", &AnalysisOptions::default());
        assert!(
            r.diagnostics
                .iter()
                .any(|d| d.code == codes::UNPARSABLE_STATEMENT),
            "invalid SQL must be reported, got: {:?}",
            r.diagnostics
        );
    }
}

#[cfg(test)]
mod tolerance_tests {
    use super::*;

    fn opts(unparsable: SkipLevel) -> AnalysisOptions {
        AnalysisOptions {
            parse: ParseConfig {
                unparsable,
                unsupported: SkipLevel::Ignore,
            },
        }
    }

    const BAD: &str = "CREATE TABLE t (a INT);\nSELCT oops;";

    #[test]
    fn default_warn_makes_invalid_sql_visible() {
        let r = analyze_source_self_contained(BAD, &AnalysisOptions::default());
        let d = r
            .diagnostics
            .iter()
            .find(|d| d.code == codes::UNPARSABLE_STATEMENT)
            .expect("an unparsable-statement diagnostic expected by default");
        assert_eq!(d.severity, Severity::Warning, "default = warn, not error");
        assert!(!r.has_errors(), "a warning must not make check fail");
    }

    #[test]
    fn ignore_restores_silence_for_migration() {
        let r = analyze_source_self_contained(BAD, &opts(SkipLevel::Ignore));
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.code == codes::UNPARSABLE_STATEMENT),
            "with ignore, nothing must be reported"
        );
        assert_eq!(r.skipped.len(), 1, "but the info stays available");
    }

    #[test]
    fn error_makes_it_fail() {
        let r = analyze_source_self_contained(BAD, &opts(SkipLevel::Error));
        assert!(r.has_errors(), "with error, check must fail");
    }

    #[test]
    fn diagnostic_points_to_the_right_line() {
        let r = analyze_source_self_contained(BAD, &AnalysisOptions::default());
        let d = r
            .diagnostics
            .iter()
            .find(|d| d.code == codes::UNPARSABLE_STATEMENT)
            .unwrap();
        let span = d.span.as_ref().expect("span expected");
        assert!(
            BAD[span.start..span.end].contains("SELCT"),
            "the span must cover the faulty statement, got {:?}",
            &BAD[span.start..span.end]
        );
    }

    #[test]
    fn comment_directive_silences_the_statement() {
        let sql = "CREATE TABLE t (a INT);\n-- pgzest-disable-next-line unparsable-statement\nSELCT oops;";
        let r = analyze_source_self_contained(sql, &opts(SkipLevel::Error));
        assert!(
            !r.has_errors(),
            "the directive must neutralize the error, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn targeted_directive_does_not_silence_other_rules() {
        // The directive only targets unparsable-statement: the column error
        // on another line must survive.
        let sql = "CREATE TABLE t (a INT);\n-- pgzest-disable-next-line unparsable-statement\nSELCT oops;\nSELECT nope FROM t;";
        let r = analyze_source_self_contained(sql, &opts(SkipLevel::Error));
        assert!(
            r.diagnostics
                .iter()
                .any(|d| d.code == codes::COLUMN_NOT_FOUND),
            "column-not-found must remain, got: {:?}",
            r.diagnostics
        );
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.code == codes::UNPARSABLE_STATEMENT),
            "unparsable must be silenced"
        );
    }

    #[test]
    fn disable_file_silences_everything() {
        let sql = "-- pgzest-disable-file\nCREATE TABLE t (a INT);\nSELECT nope FROM t;";
        let r = analyze_source_self_contained(sql, &AnalysisOptions::default());
        assert!(
            r.diagnostics.is_empty(),
            "disable-file must silence everything, got: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn clean_sql_produces_nothing() {
        let sql = "CREATE TABLE t (a INT); SELECT a FROM t;";
        let r = analyze_source_self_contained(sql, &AnalysisOptions::default());
        assert!(
            r.diagnostics.is_empty(),
            "no diagnostic expected, got: {:?}",
            r.diagnostics
        );
        assert!(r.skipped.is_empty());
    }
}

#[cfg(test)]
mod lint_tests {
    use super::*;

    #[test]
    fn lint_sql_parses_and_lints() {
        let diags = lint_sql(
            "CREATE TABLE t (a INT PRIMARY KEY);\nSELECT * FROM t;",
            &pgzest_lint::LintConfig::default(),
        );
        assert!(
            diags.iter().any(|d| d.code == "L001"),
            "L001 expected, got: {diags:?}"
        );
    }

    #[test]
    fn lint_sql_applies_directives() {
        let diags = lint_sql(
            "CREATE TABLE t (a INT PRIMARY KEY);\n-- pgzest-disable-next-line L001\nSELECT * FROM t;",
            &pgzest_lint::LintConfig::default(),
        );
        assert!(
            !diags.iter().any(|d| d.code == "L001"),
            "the directive must silence L001, got: {diags:?}"
        );
    }

    #[test]
    fn lint_sql_does_not_panic_on_invalid_sql() {
        // The former pgzest_lint::lint_sql was an unimplemented!(): every
        // call panicked. Here we must get a diagnostic, not a crash.
        let diags = lint_sql("!!! not sql !!!", &pgzest_lint::LintConfig::default());
        let _ = diags; // the only contract is: it does not panic
    }
}

// ── Sources containing macros ─────────────────────────────────────────────────

/// A source ready to analyze: the SQL, and where each fragment comes from.
pub struct PreparedSource {
    /// SQL to analyze — the source itself if it contained no macro.
    pub sql: String,
    /// Origin of each fragment of `sql`. `None` if `sql` **is** the source,
    /// in which case positions are already correct.
    pub map: Option<pgzest_ast::SourceMap>,
    /// Desugaring diagnostics, already positioned in the source.
    pub diagnostics: Vec<Diagnostic>,
    /// Desugared invocations (macros and fragments), in file order.
    pub regions: Vec<pgzest_expand::expand::ExpandedRegion>,
}

impl PreparedSource {
    /// The invocation containing `offset` (position in the source) and the
    /// SQL it generated.
    pub fn region_at(
        &self,
        offset: usize,
    ) -> Option<(&pgzest_expand::expand::ExpandedRegion, &str)> {
        let r = self.regions.iter().find(|r| r.source.contains(&offset))?;
        Some((r, self.sql.get(r.generated.clone())?))
    }

    /// Parses, bringing spans back into the original source's frame.
    ///
    /// This is where the mapping is consumed: the parser assigns spans
    /// **already translated**, so the returned AST designates the edited
    /// file. Everything reading it afterwards — checker, linter, hover,
    /// go-to-definition, codegen — gets correct positions without knowing
    /// anything about macros.
    pub fn parse(&self) -> Result<Vec<pgzest_ast::Statement>, pgzest_parser::ParseError> {
        match &self.map {
            Some(map) => pgzest_parser::parse_mapped(&self.sql, map),
            None => pgzest_parser::parse(&self.sql),
        }
    }

    /// Variant keeping track of skipped statements.
    pub fn parse_with_outcome(
        &self,
    ) -> Result<pgzest_parser::ParseOutcome, pgzest_parser::ParseError> {
        match &self.map {
            Some(map) => pgzest_parser::parse_mapped_with_outcome(&self.sql, map),
            None => pgzest_parser::parse_with_outcome(&self.sql),
        }
    }
}

/// True if the path designates a file that may contain macros.
pub fn is_macro_source(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pgsql"))
}

/// Desugars `source` if it contains macros, otherwise returns it as is.
///
/// Desugaring is attempted on every file, whatever its extension: a source
/// without macros is copied verbatim, so the check is free and no file
/// escapes processing because of its name. [`is_macro_source`] decides which
/// files to *present* as macro sources, not whether to desugar.
pub fn prepare_source(source: &str, macros: &pgzest_expand::MacroSet) -> PreparedSource {
    let expansion = pgzest_expand::expand_source(source, macros);

    if expansion.is_passthrough() {
        return PreparedSource {
            sql: source.to_string(),
            map: None,
            diagnostics: expansion.diagnostics,
            regions: Vec::new(),
        };
    }

    PreparedSource {
        sql: expansion.sql,
        map: Some(expansion.map),
        diagnostics: expansion.diagnostics,
        regions: expansion.regions,
    }
}

/// Like [`prepare_source`], for a named query file: the `$name?` markers
/// (optional parameters) are blanked after inserting the fragments.
pub fn prepare_query_source(source: &str, macros: &pgzest_expand::MacroSet) -> PreparedSource {
    let mut prepared = prepare_source(source, macros);
    prepared.sql = pgzest_query::blank_optional_markers(&prepared.sql);
    prepared
}

/// Analyzes a source that may contain macros.
///
/// Positions are in the edited file's frame, carried by the AST rather than
/// corrected afterwards: see [`PreparedSource::parse`].
pub fn analyze_source_with_macros(
    source: &str,
    macros: &pgzest_expand::MacroSet,
    schema: &ProjectCatalog,
    opts: &AnalysisOptions,
) -> AnalysisResult {
    let prepared = prepare_source(source, macros);
    let suppressions = Suppressions::scan(source);

    let outcome = match prepared.parse_with_outcome() {
        Ok(o) => o,
        Err(e) => {
            let mut diagnostics = vec![Diagnostic::error(
                codes::PARSE_ERROR,
                format!("parse error: {e}"),
            )];
            diagnostics.extend(prepared.diagnostics);
            suppressions.apply(&mut diagnostics);
            return AnalysisResult {
                diagnostics,
                skipped: Vec::new(),
                suppressions,
            };
        }
    };

    let mut diagnostics = skipped_to_diagnostics(&outcome.skipped, &opts.parse);
    diagnostics.extend(lost_to_diagnostics(&outcome.lost));
    diagnostics.extend(header_diagnostic(source));
    diagnostics.extend(crate::check_statements(&outcome.statements, schema));
    // Macro diagnostics are already in the source frame.
    diagnostics.extend(prepared.diagnostics.clone());
    suppressions.apply(&mut diagnostics);

    AnalysisResult {
        diagnostics,
        skipped: outcome.skipped,
        suppressions,
    }
}
