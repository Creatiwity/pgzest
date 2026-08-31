//! Façade d'orchestration : parse → semantic → check → lint.
//!
//! Ce crate existe pour tenir la règle de couches : `pgsql-semantic`,
//! `pgsql-checker` et `pgsql-lint` sont des passes indépendantes qui ne se
//! connaissent pas entre elles. Tout ce qui doit en enchaîner plusieurs vit
//! ici, au-dessus d'elles.
//!
//! Auparavant `analyze_sql` vivait dans `pgsql-semantic` et appelait
//! `pgsql_checker::check_statements` — une couche basse qui remontait vers une
//! couche haute, alors que la conception veut l'inverse (le checker consomme
//! le résultat sémantique).

use pgsql_diagnostics::{Diagnostic, codes};
use pgsql_schema::provider::{DdlSchemaProvider, SchemaProvider};
use pgsql_semantic::{SemanticResult, analyze};

/// Analyse `sql` et renvoie à la fois les diagnostics et le résultat sémantique.
///
/// Utile à l'outillage qui a besoin des deux : LSP (hover, complétion),
/// codegen, génération de types TypeScript.
pub fn analyze_sql(sql: &str, schema: &dyn SchemaProvider) -> (Vec<Diagnostic>, SemanticResult) {
    match pgsql_parser::parse(sql) {
        Ok(stmts) => {
            let semantic = analyze(&stmts, schema);
            let diags = pgsql_checker::check_statements(&stmts, schema);
            (diags, semantic)
        }
        Err(e) => {
            let empty = DdlSchemaProvider::from_statements(&[]);
            (
                vec![Diagnostic::error(
                    codes::PARSE_ERROR,
                    format!("parse error: {e}"),
                )],
                analyze(&[], &empty),
            )
        }
    }
}

/// Analyse `sql` en dérivant le schéma du DDL contenu dans le fichier lui-même.
///
/// Mode utilisé pour vérifier un fichier autonome, sans schéma externe.
pub fn analyze_sql_self_contained(sql: &str) -> (Vec<Diagnostic>, SemanticResult) {
    match pgsql_parser::parse(sql) {
        Ok(stmts) => {
            let schema = DdlSchemaProvider::from_statements(&stmts);
            let semantic = analyze(&stmts, &schema);
            let diags = pgsql_checker::check_statements(&stmts, &schema);
            (diags, semantic)
        }
        Err(e) => {
            let empty = DdlSchemaProvider::from_statements(&[]);
            (
                vec![Diagnostic::error(
                    codes::PARSE_ERROR,
                    format!("parse error: {e}"),
                )],
                analyze(&[], &empty),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyse_un_fichier_autonome() {
        let sql = "CREATE TABLE t (a INT); SELECT a FROM t;";
        let (diags, _semantic) = analyze_sql_self_contained(sql);
        assert!(
            diags.is_empty(),
            "aucun diagnostic attendu, obtenu: {diags:?}"
        );
    }

    #[test]
    fn signale_une_colonne_absente() {
        let sql = "CREATE TABLE t (a INT); SELECT nope FROM t;";
        let (diags, _) = analyze_sql_self_contained(sql);
        assert!(
            diags.iter().any(|d| d.code == codes::COLUMN_NOT_FOUND),
            "column-not-found attendu, obtenu: {diags:?}"
        );
    }

    /// Documente le comportement ACTUEL, qui est un défaut connu.
    ///
    /// `pgsql_parser::parse` ignore silencieusement tout statement que
    /// sqlparser-rs refuse (lib.rs:595 `Err(_) => continue`), afin de tolérer
    /// le SQL non supporté dans les dumps pg_dump. Conséquence : du SQL
    /// franchement invalide renvoie `Ok(0 statements)` et `check` conclut
    /// "aucune erreur". Le code PARSE_ERROR est donc inatteignable.
    #[test]
    fn sql_invalide_passe_silencieusement_defaut_connu() {
        let (diags, _) = analyze_sql_self_contained("SELCT * FRM;");
        assert!(
            diags.is_empty(),
            "comportement actuel : aucun diagnostic. Si ce test casse, c'est \
             que le parser signale enfin les erreurs de syntaxe — supprimer ce \
             test et activer erreur_de_parse_signalee ci-dessous."
        );
    }

    /// Comportement VOULU, pas encore implémenté. Voir le test ci-dessus.
    #[test]
    #[ignore = "le parser avale les erreurs de syntaxe (pgsql-parser/src/lib.rs:595)"]
    fn erreur_de_parse_signalee() {
        let (diags, _) = analyze_sql_self_contained("SELCT * FRM;");
        assert!(
            diags.iter().any(|d| d.code == codes::PARSE_ERROR),
            "PARSE_ERROR attendu sur du SQL invalide, obtenu: {diags:?}"
        );
    }
}
