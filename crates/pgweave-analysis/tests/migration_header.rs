//! L'en-tête de migration (PLAN.md, D9) est lu par l'analyse : mal formé, il
//! est signalé à sa ligne ; valide, il ne produit rien.

use pgweave_analysis::{AnalysisOptions, analyze_source_self_contained};
use pgweave_diagnostics::codes;

fn header_diags(sql: &str) -> Vec<pgweave_diagnostics::Diagnostic> {
    analyze_source_self_contained(sql, &AnalysisOptions::default())
        .diagnostics
        .into_iter()
        .filter(|d| d.code == codes::MIGRATION_HEADER_INVALID)
        .collect()
}

#[test]
fn un_en_tete_valide_ne_produit_rien() {
    let sql = "-- pgweave: postgres=18 extensions=citext@1.6\nCREATE TABLE t (a INT);\n";
    assert!(header_diags(sql).is_empty());
}

#[test]
fn un_en_tete_mal_forme_est_signale_a_sa_ligne() {
    let sql = "-- Migration\n-- pgweave: postgres=18 extensions=citext\nCREATE TABLE t (a INT);\n";
    let d = header_diags(sql);
    assert_eq!(d.len(), 1, "{d:?}");
    let span = d[0].span.clone().expect("span");
    assert!(sql[span.start..span.end].starts_with("-- pgweave:"));
    assert!(d[0].message.contains("nom@version"), "{}", d[0].message);
}
