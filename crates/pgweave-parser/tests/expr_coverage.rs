//! Couverture des expressions PostgreSQL courantes.
//!
//! Deux régressions distinctes sont verrouillées ici.
//!
//! **Statements écartés.** `convert_binary_op` renvoyait `Unsupported` sur 28
//! opérateurs que sqlparser sait lire, dont `||`. Un `SELECT a || b` était donc
//! retiré de l'AST — et par défaut `unsupported = "ignore"`, donc en silence.
//!
//! **AST mensonger.** Le fourre-tout `_other => Literal(Null)` transformait
//! toute expression non modélisée en `NULL`. Un `WHERE EXISTS (…)` devenait
//! `WHERE NULL`, ce qui signifie « aucune ligne » : l'AST n'omettait pas
//! l'information, il en affirmait une fausse.

use pgweave_ast::{BinaryOp, ExprKind, SelectItem, Statement};

const SCHEMA: &str = "CREATE TABLE t (a INT, b TEXT, j JSONB, ts TIMESTAMPTZ, arr INT[]);\n";

/// Parse `SCHEMA` + `sql`, renvoie le dernier statement.
fn last_stmt(sql: &str) -> Statement {
    let full = format!("{SCHEMA}{sql}");
    let out = pgweave_parser::parse_with_outcome(&full).expect("parse");
    assert!(
        out.skipped.is_empty(),
        "aucun statement ne doit être écarté, obtenu : {:?}",
        out.skipped
            .iter()
            .map(|s| (&s.sql, &s.reason))
            .collect::<Vec<_>>()
    );
    out.statements
        .into_iter()
        .next_back()
        .expect("un statement")
}

fn select_of(sql: &str) -> pgweave_ast::stmt::SelectStmt {
    match last_stmt(sql) {
        Statement::Select(s) => s,
        other => panic!("SELECT attendu, obtenu {other:?}"),
    }
}

/// Première expression projetée.
fn projection(sql: &str) -> ExprKind {
    let sel = select_of(sql);
    match sel.columns.into_iter().next().expect("une colonne") {
        SelectItem::Expr { expr, .. } => expr.kind,
        other => panic!("expression attendue, obtenu {other:?}"),
    }
}

fn where_kind(sql: &str) -> ExprKind {
    select_of(sql).where_clause.expect("WHERE attendu").kind
}

/// Vrai si l'expression, ou l'une de ses sous-expressions, est `Unsupported`.
fn has_unsupported(k: &ExprKind) -> bool {
    match k {
        ExprKind::Unsupported(_) => true,
        ExprKind::BinaryOp { left, right, .. } => {
            has_unsupported(&left.kind) || has_unsupported(&right.kind)
        }
        ExprKind::UnaryOp { expr, .. } | ExprKind::Cast { expr, .. } => has_unsupported(&expr.kind),
        _ => false,
    }
}

// ── Opérateurs auparavant fatals ──────────────────────────────────────────────

#[test]
fn la_concatenation_produit_un_operateur_dedie() {
    // `||` a sa propre variante et non `Custom` : son type de résultat est connu.
    let k = projection("SELECT b || b FROM t;");
    assert!(
        matches!(
            &k,
            ExprKind::BinaryOp {
                op: BinaryOp::Concat,
                ..
            }
        ),
        "obtenu : {k:?}"
    );
}

#[test]
fn les_operateurs_inconnus_deviennent_custom_et_non_une_erreur() {
    // Le fourre-tout route vers `Custom(symbole)` grâce au `Display` de
    // sqlparser, au lieu d'écarter le statement.
    for (sql, symbol) in [
        ("SELECT a << 2 FROM t;", "<<"),
        ("SELECT a >> 2 FROM t;", ">>"),
        ("SELECT a # 2 FROM t;", "#"),
        ("SELECT a ^ 2 FROM t;", "^"),
    ] {
        let k = projection(sql);
        match &k {
            ExprKind::BinaryOp {
                op: BinaryOp::Custom(s),
                ..
            } => assert_eq!(s, symbol, "symbole attendu pour {sql}"),
            other => panic!("Custom({symbol}) attendu pour {sql}, obtenu {other:?}"),
        }
    }
}

#[test]
fn les_operateurs_de_regex_passent() {
    for sql in [
        "SELECT a FROM t WHERE b ~ 'x';",
        "SELECT a FROM t WHERE b ~* 'x';",
        "SELECT a FROM t WHERE b !~ 'x';",
    ] {
        let k = where_kind(sql);
        assert!(matches!(&k, ExprKind::BinaryOp { .. }), "{sql} → {k:?}");
    }
}

#[test]
fn un_parametre_lie_ne_fait_plus_echouer_le_statement() {
    // Aucune requête paramétrée d'application ne passait auparavant.
    let k = where_kind("SELECT a FROM t WHERE a = $1;");
    let ExprKind::BinaryOp { right, .. } = &k else {
        panic!("BinaryOp attendu, obtenu {k:?}")
    };
    assert!(
        matches!(&right.kind, ExprKind::Parameter(p) if p == "$1"),
        "obtenu : {:?}",
        right.kind
    );
}

// ── Expressions auparavant transformées en NULL ───────────────────────────────

#[test]
fn exists_nest_plus_transforme_en_null() {
    // Le cas le plus grave : `WHERE NULL` signifie « aucune ligne ».
    let k = where_kind("SELECT a FROM t WHERE EXISTS (SELECT 1 FROM t);");
    assert!(
        matches!(&k, ExprKind::Exists { negated: false, .. }),
        "obtenu : {k:?}"
    );
}

#[test]
fn not_exists_conserve_sa_negation() {
    let k = where_kind("SELECT a FROM t WHERE NOT EXISTS (SELECT 1 FROM t);");
    assert!(
        matches!(&k, ExprKind::Exists { negated: true, .. }),
        "obtenu : {k:?}"
    );
}

#[test]
fn at_time_zone_est_conserve() {
    let k = projection("SELECT ts AT TIME ZONE 'UTC' FROM t;");
    assert!(matches!(&k, ExprKind::AtTimeZone { .. }), "obtenu : {k:?}");
}

#[test]
fn collate_est_conserve() {
    let k = projection("SELECT b COLLATE \"C\" FROM t;");
    match &k {
        ExprKind::Collate { collation, .. } => assert_eq!(collation, "C"),
        other => panic!("Collate attendu, obtenu {other:?}"),
    }
}

#[test]
fn un_acces_indice_est_conserve() {
    let k = projection("SELECT arr[1] FROM t;");
    assert!(matches!(&k, ExprKind::FieldAccess { .. }), "obtenu : {k:?}");
}

#[test]
fn extract_devient_un_appel_de_fonction() {
    // Ramené à `FunctionCall` plutôt qu'une variante par mot-clé : le nom suffit
    // à l'inférence, et la machinerie existe déjà.
    let k = projection("SELECT EXTRACT(YEAR FROM ts) FROM t;");
    match &k {
        ExprKind::FunctionCall { name, .. } => {
            let rendered = name
                .0
                .iter()
                .map(|i| i.value.as_str())
                .collect::<Vec<_>>()
                .join(".");
            assert!(rendered.contains("extract"), "obtenu : {rendered}");
        }
        other => panic!("FunctionCall attendu, obtenu {other:?}"),
    }
}

#[test]
fn un_interval_est_conserve() {
    let k = projection("SELECT ts + INTERVAL '1 day' FROM t;");
    let ExprKind::BinaryOp { right, .. } = &k else {
        panic!("BinaryOp attendu, obtenu {k:?}")
    };
    assert!(
        matches!(&right.kind, ExprKind::Interval(_)),
        "obtenu : {:?}",
        right.kind
    );
}

// ── L'échappatoire ne mente pas ───────────────────────────────────────────────

#[test]
fn une_expression_non_modelisee_se_declare_comme_telle() {
    // `GROUPING SETS` n'est pas modélisé. L'important est qu'il ne devienne pas
    // `NULL` : le nœud dit « je ne sais pas », ce qui laisse les consommateurs
    // passer leur tour au lieu de conclure de travers.
    let sel = select_of("SELECT b, count(*) FROM t GROUP BY GROUPING SETS ((b),());");
    let unsupported = sel.group_by.iter().any(|e| has_unsupported(&e.kind));
    let null_literal = sel
        .group_by
        .iter()
        .any(|e| matches!(&e.kind, ExprKind::Literal(pgweave_ast::Literal::Null)));
    assert!(
        unsupported || !null_literal,
        "ne doit pas se faire passer pour NULL : {:?}",
        sel.group_by
    );
    assert!(
        !null_literal,
        "un GROUP BY GROUPING SETS ne doit pas devenir GROUP BY NULL : {:?}",
        sel.group_by
    );
}

#[test]
fn une_expression_courante_ne_produit_aucun_unsupported() {
    // Garde-fou : les formes du quotidien doivent toutes être modélisées.
    for sql in [
        "SELECT a + 1 FROM t;",
        "SELECT b || 'x' FROM t;",
        "SELECT a FROM t WHERE a IN (1, 2);",
        "SELECT a FROM t WHERE b LIKE 'x%';",
        "SELECT CASE WHEN a > 0 THEN 'p' ELSE 'n' END FROM t;",
        "SELECT a FROM t WHERE EXISTS (SELECT 1 FROM t);",
        "SELECT lower(b) FROM t;",
        "SELECT a::TEXT FROM t;",
    ] {
        let sel = select_of(sql);
        let bad = sel
            .columns
            .iter()
            .filter_map(|c| match c {
                SelectItem::Expr { expr, .. } => Some(&expr.kind),
                _ => None,
            })
            .any(has_unsupported)
            || sel
                .where_clause
                .as_ref()
                .is_some_and(|w| has_unsupported(&w.kind));
        assert!(!bad, "{sql} ne doit produire aucun Unsupported");
    }
}
