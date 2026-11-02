mod convert;
pub mod dialect;
pub mod error;
mod grammar;
mod ids;
pub mod normalize;
mod pipeline;
pub mod plpgsql;
mod split;

pub(crate) use ids::next_node_id;

pub use error::ParseError;
pub use normalize::{NormalizedSelect, normalize_select};

use pgweave_ast::Statement;

/// Raison pour laquelle un statement n'a pas fini dans l'AST.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// sqlparser-rs n'a pas su parser le statement. À ce niveau on ne peut PAS
    /// distinguer « du PostgreSQL valide que nous ne supportons pas » (GRANT,
    /// CREATE EXTENSION, …) d'une vraie faute de syntaxe. C'est au consommateur
    /// de décider quoi en faire — voir la config `[parse] unparsable`.
    Unparsable(String),
    /// Parsé par sqlparser mais volontairement pas modélisé dans notre AST.
    /// Ici on sait que le SQL est valide, seulement hors périmètre.
    Unsupported(String),
    /// Le statement est bien dans l'AST, mais le **corps** d'une fonction ou
    /// d'une procédure PL/pgSQL n'a pas pu être parsé.
    ///
    /// Conséquence pratique : ce corps retombe sur un repli textuel presque
    /// aveugle. Les variables, les types, les `RETURN` et le contexte trigger ne
    /// sont plus vérifiés. Une seule construction non gérée suffit — d'où
    /// l'importance de le dire au lieu de laisser croire à une analyse complète.
    UnparsedBody { name: String, reason: String },
}

/// Un statement écarté pendant le parse, avec sa position dans la source.
#[derive(Debug, Clone)]
pub struct SkippedStatement {
    /// Texte du statement (sans le `;` final).
    pub sql: String,
    /// Offset octet du début du statement dans la source complète.
    pub start: usize,
    /// Offset octet de fin (exclusif).
    pub end: usize,
    pub reason: SkipReason,
}

/// Résultat complet d'un parse : ce qui a été compris, et ce qui a été écarté.
#[derive(Debug, Clone, Default)]
pub struct ParseOutcome {
    pub statements: Vec<Statement>,
    pub skipped: Vec<SkippedStatement>,
}

/// Parse `sql` et ne renvoie que les statements compris.
///
/// Les statements non parsables ou non supportés sont écartés silencieusement.
/// Préférer [`parse_with_outcome`] pour savoir ce qui a été écarté — sans quoi
/// un fichier invalide est indiscernable d'un fichier propre.
pub fn parse(sql: &str) -> Result<Vec<Statement>, ParseError> {
    Ok(parse_with_outcome(sql)?.statements)
}

/// Parse `sql` en conservant la trace des statements écartés.
pub fn parse_with_outcome(sql: &str) -> Result<ParseOutcome, ParseError> {
    parse_at_with_outcome(sql, 0)
}

/// Comment traduire un offset du texte soumis vers le repère du document.
///
/// Le parser assigne les spans **déjà traduits**, plutôt que de laisser un
/// consommateur les retraduire après coup. Conséquence : tout ce qui lit l'AST
/// — checker, linter, hover, go-to-definition, codegen — obtient des positions
/// justes, pas seulement les diagnostics.
/// Possédé plutôt qu'emprunté : la correspondance doit traverser un
/// thread-local (spans d'expression) et le parser PL/pgSQL, qui ne peuvent pas
/// porter de durée de vie. Le clone est un `Vec` de segments, négligeable
/// devant un parse.
#[derive(Clone)]
pub enum Offsets {
    /// Décalage constant : le texte soumis est un fragment contigu du document.
    Shift(usize),
    /// Correspondance quelconque : le texte soumis a été engendré depuis le
    /// document (expansion de macro, prétraitement).
    Mapped(pgweave_ast::SourceMap),
}

impl Offsets {
    /// Traduit un offset ponctuel.
    pub(crate) fn point(&self, offset: usize) -> usize {
        match self {
            Self::Shift(base) => base + offset,
            // Un point sans antécédent est rabattu sur le fragment recopié le
            // plus proche : mieux vaut une position approchée qu'aucune.
            Self::Mapped(map) => map.nearest_source(offset).unwrap_or(offset),
        }
    }

    /// Traduit une plage. La fin est bornée au segment du début, sinon une plage
    /// chevauchant du texte engendré s'étendrait absurdement dans la source.
    pub(crate) fn span(&self, start: usize, end: usize) -> (usize, usize) {
        match self {
            Self::Shift(base) => (base + start, base + end),
            Self::Mapped(map) => map
                .remap(start, end)
                .unwrap_or_else(|| (self.point(start), self.point(end))),
        }
    }
}

/// Parse un fragment SQL extrait d'un document plus grand.
///
/// `base_offset` est l'offset octet du début de `sql` dans le document
/// englobant. Tous les spans produits — statements, statements écartés, corps
/// PL/pgSQL, expressions — sont exprimés dans le repère du document.
///
/// C'est ce qui permet d'analyser du SQL qui ne vit pas dans un fichier `.sql` :
/// littéral de template dans un fichier TypeScript, bloc dans un Markdown.
///
/// [`parse`] et [`parse_with_outcome`] sont le cas `base_offset = 0`.
pub fn parse_at(sql: &str, base_offset: usize) -> Result<Vec<Statement>, ParseError> {
    Ok(parse_with_offsets(sql, Offsets::Shift(base_offset))?.statements)
}

/// Comme [`parse_at`], en conservant la trace des statements écartés.
pub fn parse_at_with_outcome(sql: &str, base_offset: usize) -> Result<ParseOutcome, ParseError> {
    parse_with_offsets(sql, Offsets::Shift(base_offset))
}

/// Parse du SQL **engendré** depuis un autre texte, spans ramenés à la source.
///
/// `map` décrit la provenance de chaque fragment de `sql`. Les spans de l'AST
/// désignent donc directement le fichier édité — un `.pgsql` par exemple — et
/// aucun consommateur n'a de retraduction à faire.
pub fn parse_mapped(sql: &str, map: &pgweave_ast::SourceMap) -> Result<Vec<Statement>, ParseError> {
    Ok(parse_with_offsets(sql, Offsets::Mapped(map.clone()))?.statements)
}

/// Comme [`parse_mapped`], en conservant la trace des statements écartés.
pub fn parse_mapped_with_outcome(
    sql: &str,
    map: &pgweave_ast::SourceMap,
) -> Result<ParseOutcome, ParseError> {
    parse_with_offsets(sql, Offsets::Mapped(map.clone()))
}

fn parse_with_offsets(sql: &str, offsets: Offsets) -> Result<ParseOutcome, ParseError> {
    pipeline::parse_text(sql, 0, &offsets)
}

pub(crate) fn set_stmt_span(stmt: &mut Statement, start: usize, end: usize) {
    use pgweave_ast::{DdlStatement, Statement::*};
    let span = pgweave_ast::node::Span {
        start: start as u32,
        end: end as u32,
    };
    match stmt {
        Select(s) => s.span = span,
        Insert(s) => s.span = span,
        Update(s) => s.span = span,
        Delete(s) => s.span = span,
        Utility(u) => u.span = span,
        Ddl(ddl) => match ddl {
            DdlStatement::CreateFunction(cf) => cf.span = span,
            DdlStatement::CreateProcedure(cp) => cp.span = span,
            DdlStatement::CreatePolicy(p) => p.span = span,
            DdlStatement::CreateTable(ct) => ct.span = span,
            DdlStatement::CreateIndex(ci) => ci.span = span,
            _ => {}
        },
    }
}

/// Expose single-statement conversion for testing purposes.
/// Takes a pre-parsed sqlparser statement and converts it to a pgweave_ast Statement.
pub fn convert_statement_pub(stmt: sqlparser::ast::Statement) -> Result<Statement, ParseError> {
    let _ids = ids::session();
    convert::convert_statement(stmt)
}

#[cfg(test)]
mod skip_tests {
    use super::*;

    #[test]
    fn sql_invalide_est_signale_pas_avale() {
        let out = parse_with_outcome("SELCT * FRM;").expect("parse ne doit pas échouer");
        assert!(out.statements.is_empty());
        assert_eq!(out.skipped.len(), 1, "un statement écarté attendu");
        assert!(matches!(out.skipped[0].reason, SkipReason::Unparsable(_)));
    }

    #[test]
    fn le_statement_ecarte_porte_sa_position() {
        let sql = "CREATE TABLE t (a INT);\nSELCT oops;";
        let out = parse_with_outcome(sql).unwrap();
        assert_eq!(out.statements.len(), 1, "le CREATE TABLE doit passer");
        assert_eq!(out.skipped.len(), 1);
        let sk = &out.skipped[0];
        // Le span doit pointer sur la 2e ligne, pas sur le début du fichier.
        assert!(
            sk.start >= 24,
            "start={} devrait être sur la 2e ligne",
            sk.start
        );
        assert!(sql[sk.start..sk.end].contains("SELCT"));
    }

    #[test]
    fn du_sql_valide_ne_produit_aucun_ecart() {
        let out = parse_with_outcome("CREATE TABLE t (a INT); SELECT a FROM t;").unwrap();
        assert_eq!(out.statements.len(), 2);
        assert!(
            out.skipped.is_empty(),
            "écarts inattendus: {:?}",
            out.skipped
        );
    }

    #[test]
    fn un_statement_invalide_ne_tue_pas_les_suivants() {
        let sql = "SELCT bad; CREATE TABLE t (a INT); SELECT a FROM t;";
        let out = parse_with_outcome(sql).unwrap();
        assert_eq!(out.statements.len(), 2);
        assert_eq!(out.skipped.len(), 1);
    }
}

#[cfg(test)]
mod parse_at_tests {
    use super::*;

    /// Span d'un statement, quel que soit son type.
    fn span_of(stmt: &Statement) -> (u32, u32) {
        use pgweave_ast::{DdlStatement, Statement::*};
        let sp = match stmt {
            Select(s) => &s.span,
            Insert(s) => &s.span,
            Update(s) => &s.span,
            Delete(s) => &s.span,
            Utility(u) => &u.span,
            Ddl(d) => match d {
                DdlStatement::CreateFunction(c) => &c.span,
                DdlStatement::CreateProcedure(c) => &c.span,
                DdlStatement::CreatePolicy(c) => &c.span,
                DdlStatement::CreateTable(c) => &c.span,
                _ => return (0, 0),
            },
        };
        (sp.start, sp.end)
    }

    #[test]
    fn base_offset_zero_equivaut_a_parse() {
        let sql = "SELECT 1;";
        let a = parse_at(sql, 0).unwrap();
        let b = parse(sql).unwrap();
        assert_eq!(span_of(&a[0]), span_of(&b[0]));
    }

    #[test]
    fn les_spans_sont_decales_du_base_offset() {
        let fragment = "SELECT 1;";
        let base = 100;
        let at_zero = parse_at(fragment, 0).unwrap();
        let at_base = parse_at(fragment, base).unwrap();

        let (s0, e0) = span_of(&at_zero[0]);
        let (sb, eb) = span_of(&at_base[0]);
        assert_eq!(sb, s0 + base as u32, "start décalé");
        assert_eq!(eb, e0 + base as u32, "end décalé");
    }

    #[test]
    fn le_span_pointe_dans_le_document_englobant() {
        // Simule du SQL extrait d'un fichier TypeScript.
        let doc = "const q = sql`\nSELECT id FROM users;\n`;\n";
        let start = doc.find("SELECT").unwrap();
        let fragment = "SELECT id FROM users;";

        let stmts = parse_at(fragment, start).unwrap();
        let (s, e) = span_of(&stmts[0]);

        // Le span doit permettre de retrouver le texte dans le document complet.
        assert_eq!(
            &doc[s as usize..e as usize],
            "SELECT id FROM users",
            "le span doit indexer le document, pas le fragment"
        );
    }

    #[test]
    fn les_statements_ecartes_sont_aussi_decales() {
        let doc = "// du TS\nSELCT oops;";
        let start = doc.find("SELCT").unwrap();
        let out = parse_at_with_outcome("SELCT oops;", start).unwrap();
        assert_eq!(out.skipped.len(), 1);
        let sk = &out.skipped[0];
        assert_eq!(
            &doc[sk.start..sk.end],
            "SELCT oops",
            "l'écart doit pointer dans le document englobant"
        );
    }

    #[test]
    fn plusieurs_statements_gardent_leurs_positions_relatives() {
        let fragment = "SELECT 1;\nSELECT 2;";
        let base = 50;
        let stmts = parse_at(fragment, base).unwrap();
        assert_eq!(stmts.len(), 2);
        let (s1, _) = span_of(&stmts[0]);
        let (s2, _) = span_of(&stmts[1]);
        assert_eq!(s1, base as u32);
        assert_eq!(s2, base as u32 + 10, "2e statement après le \\n");
    }
}

#[cfg(test)]
mod utf8_span_tests {
    use super::*;

    fn stmt_span(stmt: &Statement) -> (usize, usize) {
        use pgweave_ast::{DdlStatement, Statement::*};
        let sp = match stmt {
            Select(s) => &s.span,
            Insert(s) => &s.span,
            Update(s) => &s.span,
            Delete(s) => &s.span,
            Utility(u) => &u.span,
            Ddl(d) => match d {
                DdlStatement::CreateTable(c) => &c.span,
                DdlStatement::CreateFunction(c) => &c.span,
                DdlStatement::CreateProcedure(c) => &c.span,
                DdlStatement::CreatePolicy(c) => &c.span,
                _ => return (0, 0),
            },
        };
        (sp.start as usize, sp.end as usize)
    }

    /// Régression : `split_statements` faisait `bytes[i] as char`, réinterprétant
    /// chaque octet UTF-8 comme un code point Latin-1. `→` (3 octets) devenait
    /// 3 caractères de 2 octets, gonflant la longueur de 3 et décalant les spans
    /// de tous les statements suivants. Un simple accent dans un commentaire
    /// suffisait à faire pointer les diagnostics à côté.
    #[test]
    fn un_accent_ne_decale_pas_les_spans_suivants() {
        let sql = "-- café → thé\nSELECT nope FROM t;";
        let stmts = parse(sql).unwrap();
        assert_eq!(stmts.len(), 1);
        let (start, _) = stmt_span(&stmts[0]);
        assert_eq!(
            &sql[start..start + 6],
            "SELECT",
            "le span doit démarrer sur SELECT, pas dériver dans le mot"
        );
    }

    #[test]
    fn le_texte_non_ascii_nest_pas_corrompu() {
        // Un littéral avec des accents doit traverser le parse intact.
        let sql = "SELECT 'café crème' AS x;";
        let stmts = parse(sql).unwrap();
        assert_eq!(stmts.len(), 1);
        let (start, end) = stmt_span(&stmts[0]);
        assert_eq!(
            &sql[start..end],
            "SELECT 'café crème' AS x",
            "le texte doit être identique à la source"
        );
    }

    #[test]
    fn plusieurs_statements_apres_du_non_ascii() {
        let sql = "-- entête « accentuée »\nSELECT 1;\n-- deuxième — tiret cadratin\nSELECT 2;";
        let stmts = parse(sql).unwrap();
        assert_eq!(stmts.len(), 2);
        for st in &stmts {
            let (start, _) = stmt_span(st);
            assert_eq!(
                &sql[start..start + 6],
                "SELECT",
                "chaque span doit démarrer sur SELECT"
            );
        }
    }

    /// Le span démarre sur le SQL, pas sur les commentaires qui le précèdent.
    /// `split_statements` découpe sur `;`, donc les commentaires restent collés
    /// au statement suivant — sans ce saut, une directive placée juste avant ne
    /// couvrait pas le statement qu'elle visait.
    #[test]
    fn le_span_saute_les_commentaires_en_tete() {
        let sql = "CREATE TABLE t (a INT);\n-- un commentaire\n-- et un autre\nSELECT a FROM t;";
        let stmts = parse(sql).unwrap();
        assert_eq!(stmts.len(), 2);
        let (start, _) = stmt_span(&stmts[1]);
        assert_eq!(&sql[start..start + 6], "SELECT");
    }

    #[test]
    fn le_span_saute_aussi_les_commentaires_bloc() {
        let sql = "CREATE TABLE t (a INT);\n/* bloc\n   multiligne */\nSELECT a FROM t;";
        let stmts = parse(sql).unwrap();
        let (start, _) = stmt_span(&stmts[1]);
        assert_eq!(&sql[start..start + 6], "SELECT");
    }

    #[test]
    fn un_dollar_quote_avec_accents_reste_intact() {
        let sql = "CREATE FUNCTION f() RETURNS TEXT LANGUAGE sql AS $$ SELECT 'éà' $$;";
        let stmts = parse(sql).unwrap();
        assert_eq!(stmts.len(), 1);
        let (start, end) = stmt_span(&stmts[0]);
        assert!(
            sql[start..end].contains("'éà'"),
            "le corps dollar-quoté doit rester intact, obtenu: {:?}",
            &sql[start..end]
        );
    }
}

/// Dernier segment d'un `ObjectName`, pour nommer une fonction dans un message.
pub(crate) fn object_name_last(name: &pgweave_ast::expr::ObjectName) -> String {
    name.0
        .last()
        .map(|i| i.value.clone())
        .unwrap_or_else(|| "?".to_string())
}
