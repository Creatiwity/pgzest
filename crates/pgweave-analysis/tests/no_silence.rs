//! Le silence est un bug : rien ne doit disparaître sans le dire.
//!
//! Trois défaillances étaient muettes, chacune vérifiée ici.
//!
//! 1. Un corps PL/pgSQL non parsé retombait sur un repli textuel presque
//!    aveugle. `lib.rs` faisait `if let Ok(parsed) = …` et jetait l'erreur : une
//!    seule construction non gérée (`FOR i IN 1..3`, `ASSERT`, deux handlers
//!    `EXCEPTION`) désactivait l'analyse de toute la fonction, sans un mot.
//! 2. Un statement non modélisé était retiré de l'AST avec `unsupported`
//!    à `ignore` par défaut — donc invisible.
//! 3. Une expression non modélisée devenait `Literal(Null)`, ce qui affirmait
//!    quelque chose de faux au lieu de se taire. Traité dans expr_coverage.rs.

use pgweave_analysis::{AnalysisOptions, analyze_source_self_contained};
use pgweave_diagnostics::{Diagnostic, Severity, codes};
use pgweave_schema::{ParseConfig, SkipLevel};

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
        .unwrap_or_else(|| panic!("{code} attendu, obtenu : {diags:?}"))
}

/// Fonction PL/pgSQL avec `body` comme corps.
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

// ── 1. Corps non parsé : signalé ──────────────────────────────────────────────

/// Chaque construction ci-dessous fait retomber le corps entier en `Raw`.
/// L'important n'est pas qu'elles soient supportées, mais qu'on le DISE.
#[test]
fn un_corps_non_parse_est_signale() {
    let cases = [
        ("FOR IN entier", "    FOR i IN 1..3 LOOP n := i; END LOOP;"),
        ("ASSERT", "    ASSERT n IS NOT NULL;"),
        (
            "CASE statement",
            "    CASE n WHEN 1 THEN n := 2; ELSE n := 3; END CASE;",
        ),
        (
            "bloc DECLARE imbriqué",
            "    DECLARE m INTEGER; BEGIN m := 1; END;",
        ),
        ("label de boucle", "    <<lbl>> LOOP EXIT lbl; END LOOP;"),
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
            has(&d, codes::UNPARSED_FUNCTION_BODY),
            "{label} : le corps retombe en Raw sans être signalé — {d:?}"
        );
    }
}

#[test]
fn le_diagnostic_nomme_la_fonction_et_la_cause() {
    let d = diags(&func("    ASSERT n IS NOT NULL;"));
    let msg = &of(&d, codes::UNPARSED_FUNCTION_BODY).message;
    assert!(msg.contains("'fn'"), "la fonction doit être nommée : {msg}");
    assert!(
        msg.contains("RETURN") || msg.contains("Variables"),
        "la conséquence doit être explicite : {msg}"
    );
}

#[test]
fn un_corps_non_parse_est_un_warning_pas_une_erreur() {
    // Visible sans casser le build : c'est une lacune de l'outil, pas du SQL.
    let d = diags(&func("    ASSERT n IS NOT NULL;"));
    assert_eq!(
        of(&d, codes::UNPARSED_FUNCTION_BODY).severity,
        Severity::Warning
    );
}

#[test]
fn un_corps_qui_parse_ne_produit_rien() {
    let d = diags(&func("    n := 1;"));
    assert!(
        !has(&d, codes::UNPARSED_FUNCTION_BODY),
        "aucun signalement sur un corps valide : {d:?}"
    );
}

#[test]
fn deux_handlers_exception_font_retomber_le_corps_et_sont_signales() {
    // Cas très courant, et jusqu'ici totalement muet.
    let sql = "CREATE OR REPLACE FUNCTION fn() RETURNS INTEGER LANGUAGE plpgsql AS $$\n\
               BEGIN\n\
               \x20   RETURN 1;\n\
               EXCEPTION\n\
               \x20   WHEN unique_violation THEN RETURN 2;\n\
               \x20   WHEN others THEN RETURN 3;\n\
               END;\n\
               $$;\n";
    assert!(
        has(&diags(sql), codes::UNPARSED_FUNCTION_BODY),
        "deux handlers EXCEPTION : le corps est perdu, il faut le dire"
    );
}

#[test]
fn le_niveau_est_configurable_comme_les_autres() {
    let sql = func("    ASSERT n IS NOT NULL;");
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
        assert_eq!(found, expected, "niveau {level:?}");
    }
}

#[test]
fn une_directive_peut_taire_le_signalement() {
    // Utile quand on sait que la construction n'est pas supportée et qu'on
    // assume de ne pas analyser ce corps.
    let sql = format!(
        "-- pgweave-disable-file unparsed-function-body\n{}",
        func("    ASSERT n IS NOT NULL;")
    );
    assert!(
        !has(&diags(&sql), codes::UNPARSED_FUNCTION_BODY),
        "la directive doit fonctionner comme pour les autres codes"
    );
}

// ── 2. Statement non modélisé : visible par défaut ────────────────────────────

#[test]
fn un_statement_non_modelise_est_visible_par_defaut() {
    // Le défaut est passé de `ignore` à `warn` : un CREATE VIEW jeté signifie
    // que la vue n'existe pour aucun autre fichier, autant le dire.
    let d = diags("CREATE VIEW v AS SELECT 1 AS a;\n");
    assert!(has(&d, codes::UNSUPPORTED_STATEMENT), "obtenu : {d:?}");
    assert_eq!(
        of(&d, codes::UNSUPPORTED_STATEMENT).severity,
        Severity::Warning,
        "warning et non erreur : ça ne doit pas casser un build"
    );
}

#[test]
fn le_silence_reste_possible_pour_migrer() {
    let opts = AnalysisOptions {
        parse: ParseConfig {
            unparsable: SkipLevel::Ignore,
            unsupported: SkipLevel::Ignore,
        },
    };
    let d = analyze_source_self_contained("CREATE VIEW v AS SELECT 1 AS a;\n", &opts).diagnostics;
    assert!(
        !has(&d, codes::UNSUPPORTED_STATEMENT),
        "l'opt-out doit rester disponible : {d:?}"
    );
}

#[test]
fn du_sql_entierement_couvert_ne_produit_aucun_bruit() {
    // Garde-fou contre la sur-signalisation : ces formes doivent rester muettes.
    let sql = "CREATE TABLE t (a INT, b TEXT);\n\
               SELECT a, b || 'x' FROM t WHERE a IN (1,2);\n\
               INSERT INTO t (a) VALUES (1) ON CONFLICT (a) DO NOTHING;\n\
               UPDATE t SET b = 'y' WHERE a = 1;\n\
               DELETE FROM t WHERE a = 2;\n";
    let d = diags(sql);
    assert!(d.is_empty(), "aucun diagnostic attendu, obtenu : {d:?}");
}
