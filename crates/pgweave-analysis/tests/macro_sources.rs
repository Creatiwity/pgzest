//! Analysis of a `.pgsql` without going through `build`.
//!
//! The requirement: spans must designate the **edited** file, not the
//! generated SQL. And they must be carried by the AST, hence correct for
//! every consumer — not only for diagnostics.

use pgweave_analysis::{AnalysisOptions, analyze_source_with_macros, prepare_source};
use pgweave_catalog::ProjectCatalog;
use pgweave_diagnostics::{Diagnostic, codes};
use pgweave_macro::MacroSet;

/// Analyzes `src`, schema derived from the desugared SQL (otherwise a table
/// created by a macro would not exist).
fn analyze(src: &str) -> Vec<Diagnostic> {
    let macros = MacroSet::builtin();
    let prepared = prepare_source(src, &macros);
    let stmts = prepared.parse().expect("parse");
    let schema = ProjectCatalog::from_statements(&stmts);
    analyze_source_with_macros(src, &macros, &schema, &AnalysisOptions::default()).diagnostics
}

/// Text the span cuts out of the SOURCE.
fn slice<'a>(src: &'a str, d: &Diagnostic) -> Option<&'a str> {
    d.span
        .as_ref()
        .filter(|s| s.end > s.start && s.end <= src.len())
        .map(|s| &src[s.start..s.end])
}

fn line_of(src: &str, d: &Diagnostic) -> Option<usize> {
    d.span
        .as_ref()
        .map(|s| src[..s.start.min(src.len())].matches('\n').count() + 1)
}

fn find<'a>(diags: &'a [Diagnostic], code: &str) -> &'a Diagnostic {
    diags
        .iter()
        .find(|d| d.code == code)
        .unwrap_or_else(|| panic!("{code} expected, got: {diags:?}"))
}

const SCHEMA: &str = "CREATE TABLE users (id UUID PRIMARY KEY, email TEXT);\n";

/// Regression: diagnostics emitted from a PL/pgSQL body carried positions
/// relative to the fragment, which leaked as is — a `WHERE missing_col` in a
/// function was reported on the first line of the file. Two combined causes,
/// fixed: the `INTO` clause was *removed* from the fragment (shifting
/// everything after it), and `shift_stmt_span` only fixed the statement's
/// span, never those of nested expressions.
#[test]
fn an_error_in_a_macro_body_points_to_the_source_file() {
    let src = format!(
        "{SCHEMA}\n\
         @function count_bad(): INTEGER {{\n\
         \x20   DECLARE n INTEGER;\n\
         \x20   SELECT count(*) INTO n FROM users WHERE nope = 1;\n\
         \x20   RETURN n;\n\
         }}\n"
    );
    let diags = analyze(&src);
    let d = find(&diags, codes::COLUMN_NOT_FOUND);

    assert_eq!(
        line_of(&src, d),
        Some(5),
        ".pgsql line expected, span {:?} → {:?}",
        d.span,
        slice(&src, d)
    );
}

/// `RETURN <expr>` is checked against the declared variables: outside a
/// query, a bare identifier can only designate a variable. Body tokens keep
/// their positions: the diagnostic designates the exact identifier in the
/// `.pgsql`.
#[test]
fn a_return_of_an_unknown_variable_is_reported_in_a_pgsql() {
    let src = format!("{SCHEMA}\n@function f(): INTEGER {{\n    RETURN nope;\n}}\n");
    let diags = analyze(&src);
    let d = find(&diags, codes::UNDECLARED_VARIABLE);
    assert!(d.message.contains("nope"), "got: {}", d.message);
    assert_eq!(slice(&src, d), Some("nope"), "span {:?}", d.span);
}

/// Column precision survives desugaring, for the SQL following a macro.
#[test]
fn column_precision_survives_desugaring() {
    let src =
        format!("{SCHEMA}\n@function f(): INT {{\n RETURN 1;\n}}\n\nSELECT nope FROM users;\n");
    let diags = analyze(&src);
    assert_eq!(
        slice(&src, find(&diags, codes::COLUMN_NOT_FOUND)),
        Some("nope"),
        "a file containing a macro must not shift the SQL that follows"
    );
}

#[test]
fn a_source_without_macros_is_unchanged() {
    let src = format!("{SCHEMA}SELECT nope FROM users;\n");
    let diags = analyze(&src);
    assert_eq!(
        slice(&src, find(&diags, codes::COLUMN_NOT_FOUND)),
        Some("nope"),
        "without macros, no translation must happen"
    );
}

#[test]
fn a_correct_macro_produces_no_diagnostic() {
    let src = format!(
        "{SCHEMA}\n\
         @function count_all(): INTEGER {{\n\
         \x20   DECLARE n INTEGER;\n\
         \x20   SELECT count(*) INTO n FROM users;\n\
         \x20   RETURN n;\n\
         }}\n"
    );
    let diags = analyze(&src);
    assert!(diags.is_empty(), "no diagnostic expected, got: {diags:?}");
}

#[test]
fn a_macro_error_points_to_the_invocation() {
    let src = "@machin f() {\n  NULL;\n}\n";
    let diags = analyze(src);
    let d = find(&diags, pgweave_macro::expand::MACRO_ERROR);
    assert_eq!(line_of(src, d), Some(1));
    assert!(d.message.contains("unknown macro"));
}

#[test]
fn a_source_directive_silences_a_body_diagnostic() {
    // The directive is written in the .pgsql. It can only work if spans are
    // already in the source frame when it is applied.
    let src = format!(
        "{SCHEMA}\n\
         @function f(): INTEGER {{\n\
         \x20   -- pgweave-disable-next-line undeclared-variable\n\
         \x20   RETURN nope;\n\
         }}\n"
    );
    let diags = analyze(&src);
    assert!(
        !diags.iter().any(|d| d.code == codes::UNDECLARED_VARIABLE),
        "the directive must silence the error, got: {diags:?}"
    );
}

#[test]
fn a_multi_table_trigger_reports_the_faulty_table() {
    let src = "\
CREATE TABLE a_t (id UUID, updated_at TIMESTAMPTZ);
CREATE TABLE b_t (id UUID);

@trigger touch before update on (a_t, b_t) for each row {
    NEW.updated_at = now();
    RETURN NEW;
}
";
    let diags = analyze(src);
    let d = find(&diags, codes::COLUMN_NOT_FOUND);
    assert!(
        d.message.contains("b_t"),
        "the faulty table must be named, got: {}",
        d.message
    );
}

/// Spans are in the AST, hence correct for every consumer — not only
/// translated back on diagnostics.
#[test]
fn the_ast_directly_carries_source_spans() {
    let src = "@function f(): INT {\n    RETURN nope;\n}\n";
    let prepared = prepare_source(src, &MacroSet::builtin());
    let stmts = prepared.parse().expect("parse");

    // The function body was reparsed with the translated offset: `nope` is
    // found at the position the AST gives.
    let found = stmts.iter().any(|st| {
        if let pgweave_ast::Statement::Ddl(pgweave_ast::DdlStatement::CreateFunction(cf)) = st {
            let start = cf.body_start as usize;
            start < src.len() && src[start..].contains("RETURN nope")
        } else {
            false
        }
    });
    assert!(
        found,
        "body_start must designate the body in the .pgsql, not in the generated SQL"
    );
}

/// Each macro must produce its own diagnostic, at its own line: a single
/// mapping serves the whole file, so a composition error would make the
/// positions converge.
#[test]
fn several_macros_each_keep_their_positions() {
    let src = format!(
        "{SCHEMA}\n\
         @function a(): INT {{\n    RETURN bad_a;\n}}\n\
         \n\
         @function b(): INT {{\n    RETURN bad_b;\n}}\n"
    );
    let diags = analyze(&src);
    let lines: Vec<_> = diags
        .iter()
        .filter(|d| d.code == codes::UNDECLARED_VARIABLE)
        .map(|d| line_of(&src, d))
        .collect();
    // The two macros are on distinct lines (2 and 6): the diagnostics must
    // be too.
    assert_eq!(lines.len(), 2, "one diagnostic per macro: {diags:?}");
    assert_ne!(lines[0], lines[1], "positions must not converge: {lines:?}");
}
