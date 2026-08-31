//! Les expressions portent leur position exacte dans le document.

use pgsql_ast::{ExprKind, SelectItem, Statement};

/// Texte que le span d'une expression découpe dans la source.
fn slice<'a>(sql: &'a str, e: &pgsql_ast::Expr) -> Option<&'a str> {
    e.real_span()
        .map(|sp| &sql[sp.start as usize..sp.end as usize])
}

fn select_of(sql: &str) -> pgsql_ast::stmt::SelectStmt {
    match pgsql_parser::parse(sql).expect("parse").remove(0) {
        Statement::Select(s) => s,
        other => panic!("SELECT attendu, obtenu {other:?}"),
    }
}

fn projections(sql: &str) -> Vec<String> {
    select_of(sql)
        .columns
        .iter()
        .filter_map(|c| match c {
            SelectItem::Expr { expr, .. } => Some(
                slice(sql, expr)
                    .map(str::to_string)
                    .unwrap_or_else(|| "<sans span>".into()),
            ),
            _ => None,
        })
        .collect()
}

#[test]
fn les_colonnes_projetees_portent_leur_span() {
    let sql = "SELECT a + 1, nope FROM t;";
    assert_eq!(projections(sql), vec!["a + 1", "nope"]);
}

#[test]
fn la_clause_where_porte_son_span() {
    let sql = "SELECT a FROM t WHERE b LIKE '%x';";
    let sel = select_of(sql);
    let w = sel.where_clause.expect("WHERE attendu");
    assert_eq!(slice(sql, &w), Some("b LIKE '%x'"));
}

#[test]
fn les_sous_expressions_portent_leur_span() {
    let sql = "SELECT a FROM t WHERE b = 42;";
    let sel = select_of(sql);
    let w = sel.where_clause.expect("WHERE");
    let ExprKind::BinaryOp { left, right, .. } = &w.kind else {
        panic!("BinaryOp attendu");
    };
    assert_eq!(slice(sql, left), Some("b"));
    assert_eq!(slice(sql, right), Some("42"));
}

/// Le cas qui piégeait le parser : un caractère multi-octets avant l'expression.
/// La colonne rendue par sqlparser est en caractères, l'offset attendu en octets.
#[test]
fn un_commentaire_accentue_ne_decale_pas_les_spans() {
    let sql = "-- commentaire accentué : é à ù → ok\nSELECT a + 1, nope FROM t;";
    assert_eq!(projections(sql), vec!["a + 1", "nope"]);
}

#[test]
fn un_litteral_accentue_ne_decale_pas_la_suite() {
    let sql = "SELECT 'café', nope FROM t;";
    assert_eq!(projections(sql), vec!["'café'", "nope"]);
}

#[test]
fn les_spans_sont_relatifs_au_document_pas_au_statement() {
    let sql = "CREATE TABLE t (a INT);\nSELECT nope FROM t;";
    let stmts = pgsql_parser::parse(sql).expect("parse");
    let Statement::Select(sel) = &stmts[1] else {
        panic!("SELECT attendu en 2e position")
    };
    let SelectItem::Expr { expr, .. } = &sel.columns[0] else {
        panic!("expression attendue")
    };
    // Doit découper le document complet, pas le 2e statement isolé.
    assert_eq!(slice(sql, expr), Some("nope"));
}

#[test]
fn parse_at_decale_aussi_les_spans_dexpression() {
    // SQL extrait d'un document hôte : les spans doivent viser le document.
    let doc = "const q = sql`SELECT nope FROM t`;";
    let start = doc.find("SELECT").unwrap();
    let stmts = pgsql_parser::parse_at("SELECT nope FROM t;", start).expect("parse");
    let Statement::Select(sel) = &stmts[0] else {
        panic!("SELECT attendu")
    };
    let SelectItem::Expr { expr, .. } = &sel.columns[0] else {
        panic!("expression attendue")
    };
    let sp = expr.real_span().expect("span attendu");
    assert_eq!(&doc[sp.start as usize..sp.end as usize], "nope");
}

#[test]
fn une_expression_synthetisee_na_pas_de_span() {
    // `IS TRUE` est désucré en `= true` : le littéral n'existe pas dans la
    // source, il ne doit donc pas prétendre à une position.
    let sql = "SELECT a FROM t WHERE b IS TRUE;";
    let sel = select_of(sql);
    let w = sel.where_clause.expect("WHERE");
    let ExprKind::BinaryOp { right, .. } = &w.kind else {
        panic!("BinaryOp attendu")
    };
    assert!(
        right.real_span().is_none(),
        "le littéral synthétisé ne doit pas porter de span, obtenu {:?}",
        slice(sql, right)
    );
}
