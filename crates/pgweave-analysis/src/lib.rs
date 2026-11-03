//! Façade d'orchestration : parse → check → lint.
//!
//! Ce crate existe pour tenir la règle de couches : `pgweave-checker` (et
//! l'analyse sémantique qu'il porte) et `pgweave-lint` sont des passes
//! indépendantes qui ne se connaissent pas entre elles. Tout ce qui doit en
//! enchaîner plusieurs vit ici, au-dessus d'elles.

use pgweave_catalog::ProjectCatalog;
use pgweave_diagnostics::{Diagnostic, Severity, Span, Suppressions, codes};
use pgweave_parser::{SkipReason, SkippedStatement};
use pgweave_schema::{ParseConfig, SkipLevel};

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
             l'analyse de tout le corps ; `-- pgweave-disable-next-line \
             unparsed-function-body` pour le taire ici"
        }
        SkipReason::Unparsable(_) => {
            "si ce SQL est valide, c'est une lacune du parser : \
             `-- pgweave-disable-next-line unparsable-statement` pour le taire ici, \
             ou `[parse] unparsable = \"ignore\"` pour tout le projet"
        }
        SkipReason::Unsupported(_) => {
            "statement valide mais hors périmètre de l'analyse ; \
             `[parse] unsupported = \"ignore\"` pour ne plus le signaler"
        }
    }
}

// ── Requêtes nommées ──────────────────────────────────────────────────────────

/// Diagnostics de format d'un fichier de `[queries] directory`
/// (`docs/named-queries.md`). S'ajoutent à l'analyse habituelle du fichier,
/// qui vérifie déjà chaque statement contre le schéma.
///
/// Partagé par la CLI et le LSP : un fichier de requêtes doit être jugé de la
/// même façon dans l'éditeur et en CI.
pub fn query_file_diagnostics(
    path: &std::path::Path,
    sql: &str,
    macros: &pgweave_macro::MacroSet,
) -> Vec<Diagnostic> {
    let is_sql = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("sql"));
    if !is_sql {
        return vec![
            Diagnostic::error(
                codes::QUERY_FILE_EXTENSION,
                "un fichier de requêtes nommées est un `.sql`",
            )
            .with_hint(
                "une requête nommée utilise les fragments du projet (`@nom`), pas \
                 un fichier .pgsql",
            ),
        ];
    }
    // Les erreurs de macro sont celles du désucrage du fichier, déjà
    // rapportées avec lui.
    pgweave_query::parse_query_file_with(sql, macros)
        .diagnostics
        .into_iter()
        .filter(|d| d.code != pgweave_macro::expand::MACRO_ERROR)
        .collect()
}

/// Ce qui empêche de décrire les requêtes nommées d'un fichier (PLAN.md §4) :
/// paramètre que rien ne type, cardinalité déclarée contredite, statement
/// hors du périmètre du descripteur. Les erreurs d'analyse elles-mêmes sont
/// rapportées par la vérification du fichier, pas ici.
///
/// `here` : module du fichier dans son répertoire de requêtes ; `others` :
/// les requêtes de ce répertoire, pour vérifier une requête qui reprend les
/// lignes d'une autre (`-> autre`, `-> a::b::autre`).
pub fn query_descriptor_diagnostics(
    sql: &str,
    catalog: &ProjectCatalog,
    macros: &pgweave_macro::MacroSet,
    here: &[String],
    others: &[pgweave_query::QueryModule],
) -> Vec<Diagnostic> {
    let own = [
        codes::QUERY_PARAMETER_UNTYPED,
        codes::QUERY_CARDINALITY_MISMATCH,
        codes::QUERY_NOT_DESCRIBABLE,
    ];
    let file = pgweave_query::parse_query_file_with(sql, macros);
    let mut out = Vec::new();
    let mut described = Vec::new();
    for q in &file.queries {
        match pgweave_query::describe_named(catalog, q) {
            Ok(d) => described.push((q, d)),
            Err(ds) => out.extend(ds.into_iter().filter(|d| own.contains(&d.code))),
        }
    }
    // Ce fichier fait foi sur la version enregistrée de son module.
    let mut modules: Vec<pgweave_query::QueryModule> =
        others.iter().filter(|m| m.path != here).cloned().collect();
    modules.push(pgweave_query::QueryModule {
        source: None,
        path: here.to_vec(),
        descriptors: described.iter().map(|(_, d)| d.clone()).collect(),
    });
    for (q, d) in &described {
        let (Some(r), Some((_, range))) = (&d.row, &q.row) else {
            continue;
        };
        let target = pgweave_query::resolve_row(here, r, &modules).map(|(_, t)| t);
        if let Err(e) = pgweave_query::check_row_reference(d, target) {
            out.push(Diagnostic::error(codes::QUERY_ROW_MISMATCH, e).with_span(
                pgweave_diagnostics::Span {
                    start: range.start,
                    end: range.end,
                },
            ));
        }
    }
    out
}

/// Requêtes d'un répertoire de requêtes, par module (`order/book.sql` →
/// `["order", "book"]`). `read` donne le texte d'un fichier (document
/// ouvert dans l'éditeur, sinon disque). Une requête non décrite est
/// absente : son fichier le signale.
pub fn query_modules(
    dir: &std::path::Path,
    catalog: &ProjectCatalog,
    macros: &pgweave_macro::MacroSet,
    read: &dyn Fn(&std::path::Path) -> Option<String>,
) -> Vec<pgweave_query::QueryModule> {
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
            let descriptors = pgweave_query::parse_query_file_with(&text, macros)
                .queries
                .iter()
                .filter_map(|q| pgweave_query::describe_named(catalog, q).ok())
                .collect();
            Some(pgweave_query::QueryModule {
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

/// Parse `sql` puis le lint, directives en commentaire appliquées.
///
/// Cette fonction vit ici et pas dans `pgweave-lint` : le linter ne dépend
/// volontairement pas du parser, pour rester une passe indépendante. Ce qui
/// enchaîne parse et lint appartient donc à la façade.
///
/// Remplace l'ancien `pgweave_lint::lint_sql`, qui était un `unimplemented!()`
/// exporté publiquement — un panic garanti pour qui l'appelait.
pub fn lint_sql(sql: &str, config: &pgweave_lint::LintConfig) -> Vec<Diagnostic> {
    let Ok(stmts) = pgweave_parser::parse(sql) else {
        return vec![Diagnostic::error(
            codes::PARSE_ERROR,
            "parse error".to_string(),
        )];
    };
    let mut diags = pgweave_lint::lint_statements(&stmts, config).diagnostics;
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
    schema: &ProjectCatalog,
    opts: &AnalysisOptions,
) -> AnalysisResult {
    analyze_parsed(sql, Some(schema), opts)
}

/// Vérification des statements : contre `catalog` s'il est donné (le
/// fichier s'exécute sur ce schéma), sinon en rejouant le fichier lui-même,
/// chaque statement vérifié à son application.
fn checks(stmts: &[pgweave_ast::Statement], catalog: Option<&ProjectCatalog>) -> Vec<Diagnostic> {
    match catalog {
        Some(c) => pgweave_checker::check_statements(stmts, c),
        None => pgweave_checker::check_script(stmts),
    }
}

fn analyze_parsed(
    sql: &str,
    catalog: Option<&ProjectCatalog>,
    opts: &AnalysisOptions,
) -> AnalysisResult {
    let suppressions = Suppressions::scan(sql);

    let outcome = match pgweave_parser::parse_with_outcome(sql) {
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
    diagnostics.extend(header_diagnostic(sql));
    diagnostics.extend(checks(&outcome.statements, catalog));
    suppressions.apply(&mut diagnostics);

    AnalysisResult {
        diagnostics,
        skipped: outcome.skipped,
        suppressions,
    }
}

/// En-tête de migration mal formé (PLAN.md, D9) : un en-tête mal lu ferait
/// analyser le fichier contre un autre contexte que celui annoncé.
fn header_diagnostic(text: &str) -> Option<Diagnostic> {
    let Some(Err(e)) = pgweave_schema::MigrationHeader::find(text) else {
        return None;
    };
    Some(
        Diagnostic::error(codes::MIGRATION_HEADER_INVALID, e.message)
            .with_span(pgweave_diagnostics::Span {
                start: e.span.start,
                end: e.span.end,
            })
            .with_hint("forme attendue : `-- pgweave: postgres=18 extensions=nom@version,…`"),
    )
}

/// Variante autonome : le schéma est dérivé du DDL contenu dans la source.
pub fn analyze_source_self_contained(sql: &str, opts: &AnalysisOptions) -> AnalysisResult {
    analyze_parsed(sql, None, opts)
}

/// Diagnostics de vérification de `sql` contre `schema`.
pub fn analyze_sql(sql: &str, schema: &ProjectCatalog) -> Vec<Diagnostic> {
    match pgweave_parser::parse(sql) {
        Ok(stmts) => pgweave_checker::check_statements(&stmts, schema),
        Err(e) => vec![Diagnostic::error(
            codes::PARSE_ERROR,
            format!("parse error: {e}"),
        )],
    }
}

/// Diagnostics de vérification de `sql`, le schéma dérivé du DDL qu'il
/// contient (chaque statement vérifié à son application).
///
/// Mode utilisé pour vérifier un fichier autonome, sans schéma externe.
pub fn analyze_sql_self_contained(sql: &str) -> Vec<Diagnostic> {
    pgweave_checker::check_sql_self_contained(sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyse_un_fichier_autonome() {
        let sql = "CREATE TABLE t (a INT); SELECT a FROM t;";
        let diags = analyze_sql_self_contained(sql);
        assert!(
            diags.is_empty(),
            "aucun diagnostic attendu, obtenu: {diags:?}"
        );
    }

    #[test]
    fn signale_une_colonne_absente() {
        let sql = "CREATE TABLE t (a INT); SELECT nope FROM t;";
        let diags = analyze_sql_self_contained(sql);
        assert!(
            diags.iter().any(|d| d.code == codes::COLUMN_NOT_FOUND),
            "column-not-found attendu, obtenu: {diags:?}"
        );
    }

    /// Régression : le SQL invalide ne doit plus passer silencieusement.
    ///
    /// Historiquement `pgweave-parser` faisait `Err(_) => continue` et renvoyait
    /// `Ok(0 statements)` sur n'importe quel SQL cassé, rendant `PARSE_ERROR`
    /// inatteignable et `check` faussement rassurant. Les statements écartés
    /// sont désormais remontés, avec un niveau piloté par la config.
    #[test]
    fn sql_invalide_est_signale() {
        let _diags = analyze_sql_self_contained("SELCT * FRM;");
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
        let sql = "CREATE TABLE t (a INT);\n-- pgweave-disable-next-line unparsable-statement\nSELCT oops;";
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
        let sql = "CREATE TABLE t (a INT);\n-- pgweave-disable-next-line unparsable-statement\nSELCT oops;\nSELECT nope FROM t;";
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
        let sql = "-- pgweave-disable-file\nCREATE TABLE t (a INT);\nSELECT nope FROM t;";
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
            &pgweave_lint::LintConfig::default(),
        );
        assert!(
            diags.iter().any(|d| d.code == "L001"),
            "L001 attendu, obtenu: {diags:?}"
        );
    }

    #[test]
    fn lint_sql_applique_les_directives() {
        let diags = lint_sql(
            "CREATE TABLE t (a INT PRIMARY KEY);\n-- pgweave-disable-next-line L001\nSELECT * FROM t;",
            &pgweave_lint::LintConfig::default(),
        );
        assert!(
            !diags.iter().any(|d| d.code == "L001"),
            "la directive doit taire L001, obtenu: {diags:?}"
        );
    }

    #[test]
    fn lint_sql_ne_panique_pas_sur_du_sql_invalide() {
        // L'ancien pgweave_lint::lint_sql était un unimplemented!() : tout appel
        // paniquait. Ici on doit obtenir un diagnostic, pas un crash.
        let diags = lint_sql("!!! pas du sql !!!", &pgweave_lint::LintConfig::default());
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
    pub map: Option<pgweave_ast::SourceMap>,
    /// Diagnostics du désucrage, déjà positionnés dans la source.
    pub diagnostics: Vec<Diagnostic>,
    /// Invocations désucrées (macros et fragments), dans l'ordre du fichier.
    pub regions: Vec<pgweave_macro::expand::ExpandedRegion>,
}

impl PreparedSource {
    /// L'invocation contenant `offset` (position dans la source) et le SQL
    /// qu'elle a engendré.
    pub fn region_at(
        &self,
        offset: usize,
    ) -> Option<(&pgweave_macro::expand::ExpandedRegion, &str)> {
        let r = self.regions.iter().find(|r| r.source.contains(&offset))?;
        Some((r, self.sql.get(r.generated.clone())?))
    }

    /// Parse en ramenant les spans dans le repère de la source d'origine.
    ///
    /// C'est ici que la correspondance est consommée : le parser assigne des
    /// spans **déjà traduits**, donc l'AST rendu désigne le fichier édité. Tout
    /// ce qui le lit ensuite — checker, linter, hover, go-to-definition,
    /// codegen — obtient des positions justes sans rien savoir des macros.
    pub fn parse(&self) -> Result<Vec<pgweave_ast::Statement>, pgweave_parser::ParseError> {
        match &self.map {
            Some(map) => pgweave_parser::parse_mapped(&self.sql, map),
            None => pgweave_parser::parse(&self.sql),
        }
    }

    /// Variante conservant la trace des statements écartés.
    pub fn parse_with_outcome(
        &self,
    ) -> Result<pgweave_parser::ParseOutcome, pgweave_parser::ParseError> {
        match &self.map {
            Some(map) => pgweave_parser::parse_mapped_with_outcome(&self.sql, map),
            None => pgweave_parser::parse_with_outcome(&self.sql),
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
pub fn prepare_source(source: &str, macros: &pgweave_macro::MacroSet) -> PreparedSource {
    let expansion = pgweave_macro::expand_source(source, macros);

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

/// Comme [`prepare_source`], pour un fichier de requêtes nommées : les
/// marqueurs `$nom?` (paramètres optionnels) sont effacés après insertion
/// des fragments.
pub fn prepare_query_source(source: &str, macros: &pgweave_macro::MacroSet) -> PreparedSource {
    let mut prepared = prepare_source(source, macros);
    prepared.sql = pgweave_query::blank_optional_markers(&prepared.sql);
    prepared
}

/// Analyse une source pouvant contenir des macros.
///
/// Les positions sont dans le repère du fichier édité, portées par l'AST plutôt
/// que corrigées après coup : voir [`PreparedSource::parse`].
pub fn analyze_source_with_macros(
    source: &str,
    macros: &pgweave_macro::MacroSet,
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
    diagnostics.extend(header_diagnostic(source));
    diagnostics.extend(pgweave_checker::check_statements(
        &outcome.statements,
        schema,
    ));
    // Les diagnostics de macro sont déjà dans le repère source.
    diagnostics.extend(prepared.diagnostics.clone());
    suppressions.apply(&mut diagnostics);

    AnalysisResult {
        diagnostics,
        skipped: outcome.skipped,
        suppressions,
    }
}
