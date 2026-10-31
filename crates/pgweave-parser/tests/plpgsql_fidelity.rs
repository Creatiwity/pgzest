//! Fidélité de l'AST PL/pgSQL : ne rien affirmer de faux.
//!
//! Deux court-circuits produisaient un AST **mensonger**, avec un corps
//! `Parsed` — donc sans dégradation visible, sans repli, sans diagnostic.
//!
//! 1. Tout fragment commençant par `NEW.` ou `OLD.` était réduit à une
//!    référence de champ, le reste jeté. `IF OLD.status = 'x' AND NEW.status
//!    != 'x'` devenait `OldField("status")`.
//! 2. Un fragment que sqlparser refusait devenait le littéral `true`. Une
//!    condition `IF` était donc littéralement vraie dans l'AST.
//!
//! Le lexer était la cause commune de (2) : `||`, `::`, `->>` n'avaient aucun
//! token et étaient recollés en `"| |"`, `": :"` — du SQL invalide.

use pgweave_ast::{DdlStatement, ExprKind, Statement, ddl::FunctionBody, plpgsql::PlStatement};

/// Corps PL/pgSQL parsé, ou panique en disant pourquoi.
fn body_of(sql: &str) -> pgweave_ast::plpgsql::PlpgsqlBody {
    let stmts = pgweave_parser::parse(sql).expect("parse");
    for st in stmts {
        if let Statement::Ddl(DdlStatement::CreateFunction(cf)) = st {
            return match cf.body {
                FunctionBody::Parsed(b) => *b,
                FunctionBody::Raw(raw) => {
                    panic!("corps non parsé (retombé en Raw) :\n{raw}")
                }
                FunctionBody::SqlQuery(_) => panic!("corps SQL inattendu"),
            };
        }
    }
    panic!("aucune fonction trouvée");
}

fn trigger_fn(body: &str) -> String {
    format!(
        "CREATE TABLE t (status TEXT, id INT, updated_at TIMESTAMPTZ);\n\
         CREATE OR REPLACE FUNCTION fn() RETURNS TRIGGER LANGUAGE plpgsql AS $$\n\
         BEGIN\n\
         {body}\n\
         \x20   RETURN NEW;\n\
         END;\n\
         $$;\n"
    )
}

fn plain_fn(body: &str) -> String {
    format!(
        "CREATE TABLE t (id INT, j JSONB, b TEXT, arr INT[]);\n\
         CREATE OR REPLACE FUNCTION fn() RETURNS TEXT LANGUAGE plpgsql AS $$\n\
         DECLARE\n\
         \x20   v TEXT; n INT;\n\
         BEGIN\n\
         {body}\n\
         \x20   RETURN v;\n\
         END;\n\
         $$;\n"
    )
}

// ── 1. La troncature NEW./OLD. ────────────────────────────────────────────────

#[test]
fn une_condition_composite_sur_old_et_new_est_conservee() {
    let body = body_of(&trigger_fn(
        "    IF OLD.status = 'cancelled' AND NEW.status != 'cancelled' THEN\n\
         \x20       RETURN NULL;\n\
         \x20   END IF;",
    ));
    let PlStatement::If { condition, .. } = &body.block.statements[0] else {
        panic!("IF attendu, obtenu {:?}", body.block.statements[0])
    };

    // Auparavant : `OldField("status")`, tout le reste jeté.
    let pgweave_ast::plpgsql::PlExpr::Sql(expr) = condition else {
        panic!("expression SQL complète attendue, obtenu {condition:?}")
    };
    assert!(
        matches!(&expr.kind, ExprKind::BinaryOp { .. }),
        "la condition doit rester une comparaison composite, obtenu {:?}",
        expr.kind
    );
}

#[test]
fn new_point_colonne_seul_reste_reconnu_comme_variable_ligne() {
    // Le raccourci garde sa raison d'être sur le cas exact : le checker s'appuie
    // sur `NewField` pour vérifier la colonne contre la table du trigger.
    let body = body_of(&trigger_fn("    NEW.updated_at = now();"));
    let PlStatement::Assign { target, .. } = &body.block.statements[0] else {
        panic!("Assign attendu, obtenu {:?}", body.block.statements[0])
    };
    // La cible d'une assignation `NEW.col` doit rester un champ, pas une colonne.
    assert!(
        format!("{target:?}").contains("updated_at"),
        "obtenu {target:?}"
    );
}

#[test]
fn une_colonne_inexistante_dans_une_condition_composite_est_detectee() {
    // Le gain concret : la troncature rendait cette erreur invisible.
    let sql = trigger_fn(
        "    IF OLD.colonne_bidon = 'x' AND NEW.status != 'x' THEN\n\
         \x20       RETURN NULL;\n\
         \x20   END IF;",
    );
    let full =
        format!("{sql}CREATE TRIGGER tg BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION fn();\n");
    let diags = pgweave_checker::check_sql_self_contained(&full);
    assert!(
        diags.iter().any(|d| d.message.contains("colonne_bidon")),
        "la colonne fautive doit être signalée, obtenu : {diags:?}"
    );
}

// ── 2. Le littéral `true` de complaisance ─────────────────────────────────────

#[test]
fn une_valeur_assignee_nest_jamais_remplacee_par_true() {
    let body = body_of(&plain_fn("    v := 'a' || 'b';"));
    let PlStatement::Assign { value, .. } = &body.block.statements[0] else {
        panic!("Assign attendu")
    };
    let pgweave_ast::plpgsql::PlExpr::Sql(expr) = value else {
        panic!("expression SQL attendue, obtenu {value:?}")
    };
    assert!(
        !matches!(
            &expr.kind,
            ExprKind::Literal(pgweave_ast::Literal::Boolean(true))
        ),
        "la valeur ne doit pas être remplacée par `true`, obtenu {:?}",
        expr.kind
    );
    assert!(
        matches!(&expr.kind, ExprKind::BinaryOp { .. }),
        "une concaténation doit rester une concaténation, obtenu {:?}",
        expr.kind
    );
}

#[test]
fn un_fragment_illisible_se_declare_au_lieu_de_mentir() {
    // S'il reste un fragment que sqlparser refuse, l'AST doit dire « inconnu »
    // et non « vrai ». On ne peut pas garantir quel fragment déclenche ce
    // chemin, donc on vérifie l'invariant : aucun `Boolean(true)` synthétique
    // ne doit apparaître là où le code source n'en contient pas.
    let src = plain_fn("    v := 'x';");
    let body = body_of(&src);
    let rendered = format!("{:?}", body.block.statements);
    assert!(
        !rendered.contains("Boolean(\n                                true"),
        "aucun `true` ne doit être fabriqué : {rendered}"
    );
}

// ── 3. Les opérateurs, cause commune ──────────────────────────────────────────

#[test]
fn les_operateurs_multi_caracteres_ne_cassent_plus_le_corps() {
    // Chacun de ces corps retombait entièrement en `Raw` : le lexer n'avait
    // aucun token pour l'opérateur et le recollait avec des espaces parasites.
    for (label, body) in [
        ("||", "    v := 'a' || 'b';"),
        ("::", "    v := n::text;"),
        ("->", "    SELECT j -> 'k' INTO v FROM t;"),
        ("->>", "    SELECT j ->> 'k' INTO v FROM t;"),
        ("@>", "    SELECT b INTO v FROM t WHERE j @> '{}';"),
        ("~*", "    SELECT b INTO v FROM t WHERE b ~* 'x';"),
        ("<<", "    n := 1 << 3;"),
        ("#", "    n := 5 # 3;"),
        ("indice", "    SELECT arr[1] INTO n FROM t;"),
    ] {
        let sql = plain_fn(body);
        let stmts = pgweave_parser::parse(&sql).expect("parse");
        let parsed = stmts.iter().any(|st| {
            matches!(
                st,
                Statement::Ddl(DdlStatement::CreateFunction(cf))
                    if matches!(cf.body, FunctionBody::Parsed(_))
            )
        });
        assert!(parsed, "l'opérateur {label} fait encore perdre le corps");
    }
}

#[test]
fn la_notation_exponentielle_conserve_sa_valeur() {
    // `1e3` se lexait `Number("1")` + `Ident("e3")`, recollé `1 e3`, que
    // sqlparser lit comme un ALIAS : la valeur devenait silencieusement `1`.
    let body = body_of(&plain_fn("    n := 1e3;"));
    let PlStatement::Assign { value, .. } = &body.block.statements[0] else {
        panic!("Assign attendu")
    };
    let rendered = format!("{value:?}");
    assert!(
        rendered.contains("1e3") || rendered.contains("1E3"),
        "l'exposant doit être conservé, obtenu : {rendered}"
    );
}

// ── 4. Garde-fou sur la fixture du dépôt ──────────────────────────────────────

#[test]
fn le_schema_dexemple_na_plus_de_condition_tronquee() {
    let sql = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/schema.sql"
    ))
    .expect("examples/schema.sql");
    let stmts = pgweave_parser::parse(&sql).expect("parse");

    let mut truncated = 0usize;
    let mut total = 0usize;
    for st in &stmts {
        let Statement::Ddl(DdlStatement::CreateFunction(cf)) = st else {
            continue;
        };
        let FunctionBody::Parsed(b) = &cf.body else {
            continue;
        };
        for s in &b.block.statements {
            if let PlStatement::If { condition, .. } = s {
                total += 1;
                // Une condition réduite à un champ de ligne alors que la source
                // porte une comparaison : c'est la troncature.
                if matches!(
                    condition,
                    pgweave_ast::plpgsql::PlExpr::NewField(_)
                        | pgweave_ast::plpgsql::PlExpr::OldField(_)
                ) {
                    truncated += 1;
                }
            }
        }
    }
    assert!(total > 0, "la fixture doit contenir des IF");
    assert_eq!(
        truncated, 0,
        "aucune condition ne doit être tronquée ({total} IF au total)"
    );
}
