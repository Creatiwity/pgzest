//! Silence is a bug: nothing must disappear without saying so.
//!
//! Three failures were silent, each checked here.
//!
//! 1. An unparsed PL/pgSQL body fell back to an almost blind textual
//!    fallback, without a word. It is now reported — and the parser covers
//!    the PL/pgSQL grammar, so only an invalid body is.
//! 2. A statement not modeled was removed from the AST with `unsupported`
//!    set to `ignore` by default — hence invisible.
//! 3. An expression not modeled became `Literal(Null)`, which asserted
//!    something false instead of staying silent. Handled in expr_coverage.rs.

use pgzest_analysis::{AnalysisOptions, analyze_source_self_contained};
use pgzest_diagnostics::{Diagnostic, Severity, codes};
use pgzest_schema::{ParseConfig, SkipLevel};

fn diags(sql: &str) -> Vec<Diagnostic> {
    analyze_source_self_contained(sql, &AnalysisOptions::default()).diagnostics
}

fn has(diags: &[Diagnostic], code: &str) -> bool {
    diags.iter().any(|d| d.code == code)
}

fn of<'a>(diags: &'a [Diagnostic], code: &str) -> &'a Diagnostic {
    diags
        .iter()
        .find(|d| d.code == code)
        .unwrap_or_else(|| panic!("{code} expected, got: {diags:?}"))
}

/// PL/pgSQL function with `body` as its body.
fn func(body: &str) -> String {
    format!(
        "CREATE TABLE users (id UUID PRIMARY KEY);\n\
         CREATE OR REPLACE FUNCTION fn() RETURNS INTEGER LANGUAGE plpgsql AS $$\n\
         DECLARE\n\
         \x20   n INTEGER;\n\
         BEGIN\n\
         {body}\n\
         \x20   RETURN n;\n\
         END;\n\
         $$;\n"
    )
}

// ── 1. Unparsed body: reported ────────────────────────────────────────────────

/// Body whose PL/pgSQL is invalid: it cannot be analyzed.
const UNPARSABLE: &str = "    IF n THEN n := 1;";

/// The constructs that made the whole body fall back to `Raw` with the old
/// parser are now analyzed: nothing reported.
#[test]
fn formerly_lost_constructs_are_analyzed() {
    let cases = [
        ("integer FOR IN", "    FOR i IN 1..3 LOOP n := i; END LOOP;"),
        ("ASSERT", "    ASSERT n IS NOT NULL;"),
        (
            "CASE statement",
            "    CASE n WHEN 1 THEN n := 2; ELSE n := 3; END CASE;",
        ),
        (
            "nested DECLARE block",
            "    DECLARE m INTEGER; BEGIN m := 1; END;",
        ),
        ("loop label", "    <<lbl>> LOOP EXIT lbl; END LOOP;"),
        (
            "GET STACKED DIAGNOSTICS",
            "    GET STACKED DIAGNOSTICS n = ROW_COUNT;",
        ),
        (
            "FOREACH IN ARRAY",
            "    FOREACH n IN ARRAY ARRAY[1,2] LOOP n := n; END LOOP;",
        ),
    ];
    for (label, body) in cases {
        let d = diags(&func(body));
        assert!(
            !has(&d, codes::UNPARSED_FUNCTION_BODY),
            "{label}: the body must be analyzed — {d:?}"
        );
    }
}

#[test]
fn an_unparsed_body_is_reported() {
    let d = diags(&func(UNPARSABLE));
    assert!(has(&d, codes::UNPARSED_FUNCTION_BODY), "{d:?}");
}

#[test]
fn the_diagnostic_names_the_function_and_the_cause() {
    let d = diags(&func(UNPARSABLE));
    let msg = &of(&d, codes::UNPARSED_FUNCTION_BODY).message;
    assert!(msg.contains("'fn'"), "the function must be named: {msg}");
    assert!(
        msg.contains("RETURN") || msg.contains("Variables"),
        "the consequence must be explicit: {msg}"
    );
}

#[test]
fn an_unparsed_body_is_a_warning_not_an_error() {
    // Visible without breaking the build: it is a gap of the tool, not of the SQL.
    let d = diags(&func(UNPARSABLE));
    assert_eq!(
        of(&d, codes::UNPARSED_FUNCTION_BODY).severity,
        Severity::Warning
    );
}

#[test]
fn a_body_that_parses_produces_nothing() {
    let d = diags(&func("    n := 1;"));
    assert!(
        !has(&d, codes::UNPARSED_FUNCTION_BODY),
        "nothing reported on a valid body: {d:?}"
    );
}

#[test]
fn several_exception_handlers_are_analyzed() {
    // Very common case, long lost silently, then reported.
    let sql = "CREATE OR REPLACE FUNCTION fn() RETURNS INTEGER LANGUAGE plpgsql AS $$\n\
               BEGIN\n\
               \x20   RETURN 1;\n\
               EXCEPTION\n\
               \x20   WHEN unique_violation THEN RETURN 2;\n\
               \x20   WHEN others THEN RETURN 3;\n\
               END;\n\
               $$;\n";
    let d = diags(sql);
    assert!(!has(&d, codes::UNPARSED_FUNCTION_BODY), "{d:?}");
}

#[test]
fn the_level_is_configurable_like_the_others() {
    let sql = func(UNPARSABLE);
    for (level, expected) in [
        (SkipLevel::Ignore, None),
        (SkipLevel::Warn, Some(Severity::Warning)),
        (SkipLevel::Error, Some(Severity::Error)),
    ] {
        let opts = AnalysisOptions {
            parse: ParseConfig {
                unparsable: level,
                unsupported: SkipLevel::Ignore,
            },
        };
        let d = analyze_source_self_contained(&sql, &opts).diagnostics;
        let found = d
            .iter()
            .find(|x| x.code == codes::UNPARSED_FUNCTION_BODY)
            .map(|x| x.severity.clone());
        assert_eq!(found, expected, "level {level:?}");
    }
}

#[test]
fn a_directive_can_silence_the_report() {
    // Useful when the construct is known to be unsupported and not analyzing
    // this body is accepted.
    let sql = format!(
        "-- pgzest-disable-file unparsed-function-body\n{}",
        func(UNPARSABLE)
    );
    assert!(
        !has(&diags(&sql), codes::UNPARSED_FUNCTION_BODY),
        "the directive must work as for the other codes"
    );
}

// ── 2. Statement not modeled: visible by default ──────────────────────────────

#[test]
fn a_statement_not_modeled_is_visible_by_default() {
    // The default moved from `ignore` to `warn`: a dropped statement exists
    // for no other file, so better say it. Roles are outside the modeled
    // scope (PLAN.md, §6).
    let d = diags("CREATE ROLE app_reader;\n");
    assert!(has(&d, codes::UNSUPPORTED_STATEMENT), "got: {d:?}");
    assert_eq!(
        of(&d, codes::UNSUPPORTED_STATEMENT).severity,
        Severity::Warning,
        "warning, not error: it must not break a build"
    );
}

#[test]
fn silence_remains_possible_for_migration() {
    let opts = AnalysisOptions {
        parse: ParseConfig {
            unparsable: SkipLevel::Ignore,
            unsupported: SkipLevel::Ignore,
        },
    };
    let d = analyze_source_self_contained("CREATE ROLE app_reader;\n", &opts).diagnostics;
    assert!(
        !has(&d, codes::UNSUPPORTED_STATEMENT),
        "the opt-out must remain available: {d:?}"
    );
}

#[test]
fn fully_covered_sql_produces_no_noise() {
    // Guard against over-reporting: these forms must stay silent.
    let sql = "CREATE TABLE t (a INT, b TEXT);\n\
               SELECT a, b || 'x' FROM t WHERE a IN (1,2);\n\
               INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO NOTHING;\n\
               UPDATE t SET b = 'y' WHERE a = 1;\n\
               DELETE FROM t WHERE a = 2;\n";
    let d = diags(sql);
    assert!(d.is_empty(), "no diagnostic expected, got: {d:?}");
}
