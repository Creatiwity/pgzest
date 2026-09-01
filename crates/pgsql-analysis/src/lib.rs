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

use pgsql_diagnostics::{Diagnostic, Severity, Span, Suppressions, codes};
use pgsql_parser::{SkipReason, SkippedStatement};
use pgsql_schema::provider::{DdlSchemaProvider, SchemaProvider};
use pgsql_schema::{ParseConfig, SkipLevel};
use pgsql_semantic::{SemanticResult, analyze};

// ── Tolérance du parser ───────────────────────────────────────────────────────

/// Traduit les statements écartés par le parser en diagnostics, selon la config.
///
/// C'est ici que le choix « avaler ou signaler » se fait — plus dans le parser,
/// qui se contente désormais de rapporter ce qu'il a écarté.
pub fn skipped_to_diagnostics(skipped: &[SkippedStatement], cfg: &ParseConfig) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    for sk in skipped {
        let (level, code, label) = match &sk.reason {
            SkipReason::Unparsable(why) => (
                cfg.unparsable,
                codes::UNPARSABLE_STATEMENT,
                format!("statement illisible, ignoré par l'analyse : {why}"),
            ),
            SkipReason::Unsupported(what) => (
                cfg.unsupported,
                codes::UNSUPPORTED_STATEMENT,
                format!("statement non modélisé, ignoré par l'analyse : {what}"),
            ),
            // Le statement est bien analysé, mais pas le corps de la fonction.
            // Même niveau que `unparsable` : dans les deux cas, il y a du SQL
            // que nous n'avons pas su lire.
            SkipReason::UnparsedBody { name, reason } => (
                cfg.unparsable,
                codes::UNPARSED_FUNCTION_BODY,
                format!(
                    "corps de '{name}' non analysé : {reason}. \
                     Variables, types, RETURN et contexte trigger ne sont pas vérifiés"
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

fn hint_for(reason: &SkipReason) -> &'static str {
    match reason {
        SkipReason::UnparsedBody { .. } => {
            "une seule construction PL/pgSQL non gérée suffit à désactiver \
             l'analyse de tout le corps ; `-- pgsql-disable-next-line \
             unparsed-function-body` pour le taire ici"
        }
        SkipReason::Unparsable(_) => {
            "si ce SQL est valide, c'est une lacune du parser : \
             `-- pgsql-disable-next-line unparsable-statement` pour le taire ici, \
             ou `[parse] unparsable = \"ignore\"` pour tout le projet"
        }
        SkipReason::Unsupported(_) => {
            "statement valide mais hors périmètre de l'analyse ; \
             `[parse] unsupported = \"ignore\"` pour ne plus le signaler"
        }
    }
}

// ── Lint ──────────────────────────────────────────────────────────────────────

/// Parse `sql` puis le lint, directives en commentaire appliquées.
///
/// Cette fonction vit ici et pas dans `pgsql-lint` : le linter ne dépend
/// volontairement pas du parser, pour rester une passe indépendante. Ce qui
/// enchaîne parse et lint appartient donc à la façade.
///
/// Remplace l'ancien `pgsql_lint::lint_sql`, qui était un `unimplemented!()`
/// exporté publiquement — un panic garanti pour qui l'appelait.
pub fn lint_sql(sql: &str, config: &pgsql_lint::LintConfig) -> Vec<Diagnostic> {
    let Ok(stmts) = pgsql_parser::parse(sql) else {
        return vec![Diagnostic::error(
            codes::PARSE_ERROR,
            "parse error".to_string(),
        )];
    };
    let mut diags = pgsql_lint::lint_statements(&stmts, config).diagnostics;
    Suppressions::scan(sql).apply(&mut diags);
    diags
}

// ── Analyse complète ──────────────────────────────────────────────────────────

/// Options d'une analyse.
#[derive(Debug, Clone, Default)]
pub struct AnalysisOptions {
    pub parse: ParseConfig,
}

/// Résultat d'une analyse complète.
pub struct AnalysisResult {
    /// Diagnostics, directives en commentaire déjà appliquées.
    pub diagnostics: Vec<Diagnostic>,
    pub semantic: SemanticResult,
    /// Statements écartés par le parser, quel que soit le niveau configuré.
    pub skipped: Vec<SkippedStatement>,
    /// Directives trouvées dans la source.
    pub suppressions: Suppressions,
}

impl AnalysisResult {
    /// Vrai si au moins un diagnostic est de sévérité Error.
    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|d| d.severity == Severity::Error)
    }
}

/// Pipeline complet sur une source : parse → tolérance → semantic → check,
/// puis application des directives en commentaire.
///
/// C'est l'entrée à privilégier pour la CLI, le LSP et les bindings : elle
/// garantit que tout le monde applique la même politique de tolérance et les
/// mêmes suppressions.
pub fn analyze_source(
    sql: &str,
    schema: &dyn SchemaProvider,
    opts: &AnalysisOptions,
) -> AnalysisResult {
    let suppressions = Suppressions::scan(sql);

    let outcome = match pgsql_parser::parse_with_outcome(sql) {
        Ok(o) => o,
        Err(e) => {
            let mut diagnostics = vec![Diagnostic::error(
                codes::PARSE_ERROR,
                format!("parse error: {e}"),
            )];
            suppressions.apply(&mut diagnostics);
            let empty = DdlSchemaProvider::from_statements(&[]);
            return AnalysisResult {
                diagnostics,
                semantic: analyze(&[], &empty),
                skipped: Vec::new(),
                suppressions,
            };
        }
    };

    let semantic = analyze(&outcome.statements, schema);
    let mut diagnostics = skipped_to_diagnostics(&outcome.skipped, &opts.parse);
    diagnostics.extend(pgsql_checker::check_statements(&outcome.statements, schema));
    suppressions.apply(&mut diagnostics);

    AnalysisResult {
        diagnostics,
        semantic,
        skipped: outcome.skipped,
        suppressions,
    }
}

/// Variante autonome : le schéma est dérivé du DDL contenu dans la source.
pub fn analyze_source_self_contained(sql: &str, opts: &AnalysisOptions) -> AnalysisResult {
    match pgsql_parser::parse(sql) {
        Ok(stmts) => {
            let schema = DdlSchemaProvider::from_statements(&stmts);
            analyze_source(sql, &schema, opts)
        }
        Err(_) => {
            let empty = DdlSchemaProvider::from_statements(&[]);
            analyze_source(sql, &empty, opts)
        }
    }
}

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

    /// Régression : le SQL invalide ne doit plus passer silencieusement.
    ///
    /// Historiquement `pgsql-parser` faisait `Err(_) => continue` et renvoyait
    /// `Ok(0 statements)` sur n'importe quel SQL cassé, rendant `PARSE_ERROR`
    /// inatteignable et `check` faussement rassurant. Les statements écartés
    /// sont désormais remontés, avec un niveau piloté par la config.
    #[test]
    fn sql_invalide_est_signale() {
        let (_diags, _) = analyze_sql_self_contained("SELCT * FRM;");
        // analyze_sql_self_contained garde le contrat historique (pas de config) ;
        // c'est analyze_source qui applique la politique de tolérance.
        let r = analyze_source_self_contained("SELCT * FRM;", &AnalysisOptions::default());
        assert!(
            r.diagnostics
                .iter()
                .any(|d| d.code == codes::UNPARSABLE_STATEMENT),
            "le SQL invalide doit être signalé, obtenu: {:?}",
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
    fn defaut_warn_le_sql_invalide_devient_visible() {
        let r = analyze_source_self_contained(BAD, &AnalysisOptions::default());
        let d = r
            .diagnostics
            .iter()
            .find(|d| d.code == codes::UNPARSABLE_STATEMENT)
            .expect("un diagnostic unparsable-statement attendu par défaut");
        assert_eq!(d.severity, Severity::Warning, "défaut = warn, pas error");
        assert!(
            !r.has_errors(),
            "un warning ne doit pas faire échouer check"
        );
    }

    #[test]
    fn ignore_restaure_le_silence_pour_migrer() {
        let r = analyze_source_self_contained(BAD, &opts(SkipLevel::Ignore));
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.code == codes::UNPARSABLE_STATEMENT),
            "en ignore, rien ne doit être signalé"
        );
        assert_eq!(r.skipped.len(), 1, "mais l'info reste disponible");
    }

    #[test]
    fn error_fait_echouer() {
        let r = analyze_source_self_contained(BAD, &opts(SkipLevel::Error));
        assert!(r.has_errors(), "en error, check doit échouer");
    }

    #[test]
    fn le_diagnostic_pointe_sur_la_bonne_ligne() {
        let r = analyze_source_self_contained(BAD, &AnalysisOptions::default());
        let d = r
            .diagnostics
            .iter()
            .find(|d| d.code == codes::UNPARSABLE_STATEMENT)
            .unwrap();
        let span = d.span.as_ref().expect("span attendu");
        assert!(
            BAD[span.start..span.end].contains("SELCT"),
            "le span doit couvrir le statement fautif, obtenu {:?}",
            &BAD[span.start..span.end]
        );
    }

    #[test]
    fn directive_en_commentaire_taire_le_statement() {
        let sql =
            "CREATE TABLE t (a INT);\n-- pgsql-disable-next-line unparsable-statement\nSELCT oops;";
        let r = analyze_source_self_contained(sql, &opts(SkipLevel::Error));
        assert!(
            !r.has_errors(),
            "la directive doit neutraliser l'erreur, obtenu: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn directive_ciblee_ne_taire_pas_les_autres_regles() {
        // La directive ne vise que unparsable-statement : l'erreur de colonne
        // sur une autre ligne doit survivre.
        let sql = "CREATE TABLE t (a INT);\n-- pgsql-disable-next-line unparsable-statement\nSELCT oops;\nSELECT nope FROM t;";
        let r = analyze_source_self_contained(sql, &opts(SkipLevel::Error));
        assert!(
            r.diagnostics
                .iter()
                .any(|d| d.code == codes::COLUMN_NOT_FOUND),
            "column-not-found doit rester, obtenu: {:?}",
            r.diagnostics
        );
        assert!(
            !r.diagnostics
                .iter()
                .any(|d| d.code == codes::UNPARSABLE_STATEMENT),
            "unparsable doit être tu"
        );
    }

    #[test]
    fn disable_file_taire_tout() {
        let sql = "-- pgsql-disable-file\nCREATE TABLE t (a INT);\nSELECT nope FROM t;";
        let r = analyze_source_self_contained(sql, &AnalysisOptions::default());
        assert!(
            r.diagnostics.is_empty(),
            "disable-file doit tout taire, obtenu: {:?}",
            r.diagnostics
        );
    }

    #[test]
    fn sql_propre_ne_produit_rien() {
        let sql = "CREATE TABLE t (a INT); SELECT a FROM t;";
        let r = analyze_source_self_contained(sql, &AnalysisOptions::default());
        assert!(
            r.diagnostics.is_empty(),
            "aucun diagnostic attendu, obtenu: {:?}",
            r.diagnostics
        );
        assert!(r.skipped.is_empty());
    }
}

#[cfg(test)]
mod lint_tests {
    use super::*;

    #[test]
    fn lint_sql_parse_et_lint() {
        let diags = lint_sql(
            "CREATE TABLE t (a INT PRIMARY KEY);\nSELECT * FROM t;",
            &pgsql_lint::LintConfig::default(),
        );
        assert!(
            diags.iter().any(|d| d.code == "L001"),
            "L001 attendu, obtenu: {diags:?}"
        );
    }

    #[test]
    fn lint_sql_applique_les_directives() {
        let diags = lint_sql(
            "CREATE TABLE t (a INT PRIMARY KEY);\n-- pgsql-disable-next-line L001\nSELECT * FROM t;",
            &pgsql_lint::LintConfig::default(),
        );
        assert!(
            !diags.iter().any(|d| d.code == "L001"),
            "la directive doit taire L001, obtenu: {diags:?}"
        );
    }

    #[test]
    fn lint_sql_ne_panique_pas_sur_du_sql_invalide() {
        // L'ancien pgsql_lint::lint_sql était un unimplemented!() : tout appel
        // paniquait. Ici on doit obtenir un diagnostic, pas un crash.
        let diags = lint_sql("!!! pas du sql !!!", &pgsql_lint::LintConfig::default());
        let _ = diags; // le seul contrat est : ça ne panique pas
    }
}

// ── Sources contenant des macros ──────────────────────────────────────────────

/// Une source prête à analyser : le SQL, et d'où vient chaque fragment.
pub struct PreparedSource {
    /// SQL à analyser — la source elle-même si elle ne contenait aucune macro.
    pub sql: String,
    /// Provenance de chaque fragment de `sql`. `None` si `sql` **est** la
    /// source, auquel cas les positions sont déjà les bonnes.
    pub map: Option<pgsql_ast::SourceMap>,
    /// Diagnostics du désucrage, déjà positionnés dans la source.
    pub diagnostics: Vec<Diagnostic>,
}

impl PreparedSource {
    /// Parse en ramenant les spans dans le repère de la source d'origine.
    ///
    /// C'est ici que la correspondance est consommée : le parser assigne des
    /// spans **déjà traduits**, donc l'AST rendu désigne le fichier édité. Tout
    /// ce qui le lit ensuite — checker, linter, hover, go-to-definition,
    /// codegen — obtient des positions justes sans rien savoir des macros.
    pub fn parse(&self) -> Result<Vec<pgsql_ast::Statement>, pgsql_parser::ParseError> {
        match &self.map {
            Some(map) => pgsql_parser::parse_mapped(&self.sql, map),
            None => pgsql_parser::parse(&self.sql),
        }
    }

    /// Variante conservant la trace des statements écartés.
    pub fn parse_with_outcome(
        &self,
    ) -> Result<pgsql_parser::ParseOutcome, pgsql_parser::ParseError> {
        match &self.map {
            Some(map) => pgsql_parser::parse_mapped_with_outcome(&self.sql, map),
            None => pgsql_parser::parse_with_outcome(&self.sql),
        }
    }
}

/// Vrai si le chemin désigne un fichier pouvant contenir des macros.
pub fn is_macro_source(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("pgsql"))
}

/// Désucre `source` si elle contient des macros, sinon la renvoie telle quelle.
///
/// Le désucrage est tenté sur tout fichier, quelle qu'en soit l'extension : une
/// source sans macro est recopiée à l'identique, donc la vérification est
/// gratuite et aucun fichier n'échappe au traitement à cause de son nom.
/// [`is_macro_source`] sert à décider quels fichiers *présenter* comme sources
/// de macros, pas à décider s'il faut désucrer.
pub fn prepare_source(source: &str, macros: &pgsql_macro::MacroSet) -> PreparedSource {
    let expansion = pgsql_macro::expand_source(source, macros);

    if expansion.is_passthrough() {
        return PreparedSource {
            sql: source.to_string(),
            map: None,
            diagnostics: expansion.diagnostics,
        };
    }

    PreparedSource {
        sql: expansion.sql,
        map: Some(expansion.map),
        diagnostics: expansion.diagnostics,
    }
}

/// Analyse une source pouvant contenir des macros.
///
/// Les positions sont dans le repère du fichier édité, portées par l'AST plutôt
/// que corrigées après coup : voir [`PreparedSource::parse`].
pub fn analyze_source_with_macros(
    source: &str,
    macros: &pgsql_macro::MacroSet,
    schema: &dyn SchemaProvider,
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
            let empty = DdlSchemaProvider::from_statements(&[]);
            return AnalysisResult {
                diagnostics,
                semantic: analyze(&[], &empty),
                skipped: Vec::new(),
                suppressions,
            };
        }
    };

    let semantic = analyze(&outcome.statements, schema);
    let mut diagnostics = skipped_to_diagnostics(&outcome.skipped, &opts.parse);
    diagnostics.extend(pgsql_checker::check_statements(&outcome.statements, schema));
    // Les diagnostics de macro sont déjà dans le repère source.
    diagnostics.extend(prepared.diagnostics.clone());
    suppressions.apply(&mut diagnostics);

    AnalysisResult {
        diagnostics,
        semantic,
        skipped: outcome.skipped,
        suppressions,
    }
}
