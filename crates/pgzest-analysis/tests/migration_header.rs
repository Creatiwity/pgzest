//! The migration header (PLAN.md, D9) is read by the analysis: malformed, it
//! is reported at its line; valid, it produces nothing.

use pgzest_analysis::{AnalysisOptions, analyze_source_self_contained};
use pgzest_diagnostics::codes;

fn header_diags(sql: &str) -> Vec<pgzest_diagnostics::Diagnostic> {
    analyze_source_self_contained(sql, &AnalysisOptions::default())
        .diagnostics
        .into_iter()
        .filter(|d| d.code == codes::MIGRATION_HEADER_INVALID)
        .collect()
}

#[test]
fn a_valid_header_produces_nothing() {
    let sql = "-- pgzest: postgres=18 extensions=citext@1.6\nCREATE TABLE t (a INT);\n";
    assert!(header_diags(sql).is_empty());
}

#[test]
fn a_malformed_header_is_reported_at_its_line() {
    let sql = "-- Migration\n-- pgzest: postgres=18 extensions=citext\nCREATE TABLE t (a INT);\n";
    let d = header_diags(sql);
    assert_eq!(d.len(), 1, "{d:?}");
    let span = d[0].span.clone().expect("span");
    assert!(sql[span.start..span.end].starts_with("-- pgzest:"));
    assert!(d[0].message.contains("name@version"), "{}", d[0].message);
}
