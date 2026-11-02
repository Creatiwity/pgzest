//! Analyse d'un `.pgsql` sans passer par `build`.
//!
//! L'exigence : les spans doivent désigner le fichier **édité**, pas le SQL
//! engendré. Et ils doivent être portés par l'AST, donc justes pour tout
//! consommateur — pas seulement pour les diagnostics.

use pgweave_analysis::{AnalysisOptions, analyze_source_with_macros, prepare_source};
use pgweave_catalog::ProjectCatalog;
use pgweave_diagnostics::{Diagnostic, codes};
use pgweave_macro::MacroSet;

/// Analyse `src`, schéma dérivé du SQL désucré (sinon une table créée par une
/// macro n'existerait pas).
fn analyze(src: &str) -> Vec<Diagnostic> {
    let macros = MacroSet::builtin();
    let prepared = prepare_source(src, &macros);
    let stmts = prepared.parse().expect("parse");
    let schema = ProjectCatalog::from_statements(&stmts);
    analyze_source_with_macros(src, &macros, &schema, &AnalysisOptions::default()).diagnostics
}

/// Texte que le span découpe dans la SOURCE.
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
        .unwrap_or_else(|| panic!("{code} attendu, obtenu : {diags:?}"))
}

const SCHEMA: &str = "CREATE TABLE users (id UUID PRIMARY KEY, email TEXT);\n";

/// Régression : les diagnostics émis depuis un corps PL/pgSQL portaient des
/// positions relatives au fragment, qui fuyaient telles quelles — un
/// `WHERE col_inexistante` dans une fonction était rapporté sur la première
/// ligne du fichier. Deux causes cumulées, corrigées :
/// la clause `INTO` était *retirée* du fragment (décalant tout ce qui suit), et
/// `shift_stmt_span` ne corrigeait que le span du statement, jamais ceux des
/// expressions imbriquées.
#[test]
fn une_erreur_dans_un_corps_de_macro_pointe_le_fichier_source() {
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
        "ligne du .pgsql attendue, span {:?} → {:?}",
        d.span,
        slice(&src, d)
    );
}

/// `RETURN <expr>` est confronté aux variables déclarées : hors requête, un
/// identifiant nu ne peut désigner qu'une variable. Les tokens du corps
/// gardent leurs positions : le diagnostic désigne l'identifiant exact dans
/// le `.pgsql`.
#[test]
fn un_return_dune_variable_inconnue_est_signale_dans_un_pgsql() {
    let src = format!("{SCHEMA}\n@function f(): INTEGER {{\n    RETURN nope;\n}}\n");
    let diags = analyze(&src);
    let d = find(&diags, codes::UNDECLARED_VARIABLE);
    assert!(d.message.contains("nope"), "obtenu : {}", d.message);
    assert_eq!(slice(&src, d), Some("nope"), "span {:?}", d.span);
}

/// La précision à la colonne survit au désucrage, pour le SQL qui suit une
/// macro.
#[test]
fn la_precision_colonne_survit_au_desucrage() {
    let src =
        format!("{SCHEMA}\n@function f(): INT {{\n RETURN 1;\n}}\n\nSELECT nope FROM users;\n");
    let diags = analyze(&src);
    assert_eq!(
        slice(&src, find(&diags, codes::COLUMN_NOT_FOUND)),
        Some("nope"),
        "un fichier contenant une macro ne doit pas décaler le SQL qui suit"
    );
}

#[test]
fn une_source_sans_macro_est_inchangee() {
    let src = format!("{SCHEMA}SELECT nope FROM users;\n");
    let diags = analyze(&src);
    assert_eq!(
        slice(&src, find(&diags, codes::COLUMN_NOT_FOUND)),
        Some("nope"),
        "sans macro, aucune traduction ne doit intervenir"
    );
}

#[test]
fn une_macro_correcte_ne_produit_aucun_diagnostic() {
    let src = format!(
        "{SCHEMA}\n\
         @function count_all(): INTEGER {{\n\
         \x20   DECLARE n INTEGER;\n\
         \x20   SELECT count(*) INTO n FROM users;\n\
         \x20   RETURN n;\n\
         }}\n"
    );
    let diags = analyze(&src);
    assert!(
        diags.is_empty(),
        "aucun diagnostic attendu, obtenu : {diags:?}"
    );
}

#[test]
fn une_erreur_de_macro_pointe_linvocation() {
    let src = "@machin f() {\n  NULL;\n}\n";
    let diags = analyze(src);
    let d = find(&diags, pgweave_macro::expand::MACRO_ERROR);
    assert_eq!(line_of(src, d), Some(1));
    assert!(d.message.contains("inconnue"));
}

#[test]
fn une_directive_de_la_source_taire_un_diagnostic_du_corps() {
    // La directive est écrite dans le .pgsql. Elle ne peut fonctionner que si
    // les spans sont déjà dans le repère source quand on l'applique.
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
        "la directive doit taire l'erreur, obtenu : {diags:?}"
    );
}

#[test]
fn un_trigger_multi_tables_signale_la_table_fautive() {
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
        "la table fautive doit être nommée, obtenu : {}",
        d.message
    );
}

/// Les spans sont dans l'AST, donc justes pour tout consommateur — pas
/// seulement retraduits sur les diagnostics.
#[test]
fn last_porte_directement_les_spans_source() {
    let src = "@function f(): INT {\n    RETURN nope;\n}\n";
    let prepared = prepare_source(src, &MacroSet::builtin());
    let stmts = prepared.parse().expect("parse");

    // Le corps de la fonction a été reparsé avec l'offset traduit : on
    // retrouve `nope` à la position que l'AST indique.
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
        "body_start doit désigner le corps dans le .pgsql, pas dans le SQL engendré"
    );
}

/// Chaque macro doit produire son propre diagnostic, à sa propre ligne : une
/// seule correspondance sert tout le fichier, donc une erreur de composition
/// ferait converger les positions.
#[test]
fn plusieurs_macros_gardent_chacune_leurs_positions() {
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
    // Les deux macros sont sur des lignes distinctes (2 et 6) : les
    // diagnostics doivent l'être aussi.
    assert_eq!(lines.len(), 2, "un diagnostic par macro : {diags:?}");
    assert_ne!(
        lines[0], lines[1],
        "les positions ne doivent pas converger : {lines:?}"
    );
}
