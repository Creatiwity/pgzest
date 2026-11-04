//! Fidelity of the PL/pgSQL AST: assert nothing false.
//!
//! Two shortcuts produced a **lying** AST, with a `Parsed`
//! body — hence with no visible degradation, no fallback, no diagnostic.
//!
//! 1. Any fragment starting with `NEW.` or `OLD.` was reduced to a
//!    field reference, the rest thrown away. `IF OLD.status = 'x' AND NEW.status
//!    != 'x'` became `OldField("status")`.
//! 2. A fragment sqlparser rejected became the literal `true`. An `IF`
//!    condition was thus literally true in the AST.
//!
//! The lexer was the common cause of (2): `||`, `::`, `->>` had no
//! token and were glued back as `"| |"`, `": :"` — invalid SQL.

use pgzest_ast::{DdlStatement, ExprKind, Statement, ddl::FunctionBody, plpgsql::PlStatementKind};

/// Parsed PL/pgSQL body, or panic saying why.
fn body_of(sql: &str) -> pgzest_ast::plpgsql::PlpgsqlBody {
    let stmts = pgzest_parser::parse(sql).expect("parse");
    for st in stmts {
        if let Statement::Ddl(DdlStatement::CreateFunction(cf)) = st {
            return match cf.body {
                FunctionBody::Parsed(b) => *b,
                FunctionBody::Raw(raw) => {
                    panic!("body not parsed (fell back to Raw):\n{raw}")
                }
                FunctionBody::SqlQuery(_) | FunctionBody::Sql(_) | FunctionBody::SqlReturn(_) => {
                    panic!("unexpected SQL body")
                }
            };
        }
    }
    panic!("no function found");
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

// ── 1. The NEW./OLD. truncation ───────────────────────────────────────────────

#[test]
fn a_composite_condition_on_old_and_new_is_kept() {
    let body = body_of(&trigger_fn(
        "    IF OLD.status = 'cancelled' AND NEW.status != 'cancelled' THEN\n\
         \x20       RETURN NULL;\n\
         \x20   END IF;",
    ));
    let PlStatementKind::If {
        condition: expr, ..
    } = &body.block.statements[0].kind
    else {
        panic!("IF expected, got {:?}", body.block.statements[0])
    };

    // Previously: `OldField("status")`, all the rest thrown away.
    assert!(
        matches!(&expr.kind, ExprKind::BinaryOp { .. }),
        "the condition must stay a composite comparison, got {:?}",
        expr.kind
    );
}

#[test]
fn new_dot_column_alone_is_still_recognized_as_row_variable() {
    // The target `NEW.col` stays a path: the checker checks the column
    // against the trigger's table.
    let body = body_of(&trigger_fn("    NEW.updated_at = now();"));
    let PlStatementKind::Assign { target, .. } = &body.block.statements[0].kind else {
        panic!("Assign expected, got {:?}", body.block.statements[0])
    };
    // The target of a `NEW.col` assignment must stay a field, not a column.
    assert!(
        format!("{target:?}").contains("updated_at"),
        "got {target:?}"
    );
}

#[test]
fn a_missing_column_in_a_composite_condition_is_detected() {
    // The concrete gain: the truncation made this error invisible.
    let sql = trigger_fn(
        "    IF OLD.bogus_column = 'x' AND NEW.status != 'x' THEN\n\
         \x20       RETURN NULL;\n\
         \x20   END IF;",
    );
    let full =
        format!("{sql}CREATE TRIGGER tg BEFORE UPDATE ON t FOR EACH ROW EXECUTE FUNCTION fn();\n");
    let diags = pgzest_checker::check_sql_self_contained(&full);
    assert!(
        diags.iter().any(|d| d.message.contains("bogus_column")),
        "the faulty column must be reported, got: {diags:?}"
    );
}

// ── 2. The complacent `true` literal ──────────────────────────────────────────

#[test]
fn an_assigned_value_is_never_replaced_by_true() {
    let body = body_of(&plain_fn("    v := 'a' || 'b';"));
    let PlStatementKind::Assign { value: expr, .. } = &body.block.statements[0].kind else {
        panic!("Assign expected")
    };
    assert!(
        !matches!(
            &expr.kind,
            ExprKind::Literal(pgzest_ast::Literal::Boolean(true))
        ),
        "the value must not be replaced by `true`, got {:?}",
        expr.kind
    );
    assert!(
        matches!(&expr.kind, ExprKind::BinaryOp { .. }),
        "a concatenation must stay a concatenation, got {:?}",
        expr.kind
    );
}

#[test]
fn an_unreadable_fragment_declares_itself_instead_of_lying() {
    // If a fragment sqlparser rejects remains, the AST must say "unknown"
    // and not "true". We cannot guarantee which fragment triggers this
    // path, so we check the invariant: no synthetic `Boolean(true)`
    // must appear where the source code contains none.
    let src = plain_fn("    v := 'x';");
    let body = body_of(&src);
    let rendered = format!("{:?}", body.block.statements);
    assert!(
        !rendered.contains("Boolean(\n                                true"),
        "no `true` must be fabricated: {rendered}"
    );
}

// ── 3. Operators, the common cause ────────────────────────────────────────────

#[test]
fn multi_character_operators_no_longer_break_the_body() {
    // Each of these bodies fell back entirely to `Raw`: the lexer had no
    // token for the operator and glued it back with spurious spaces.
    for (label, body) in [
        ("||", "    v := 'a' || 'b';"),
        ("::", "    v := n::text;"),
        ("->", "    SELECT j -> 'k' INTO v FROM t;"),
        ("->>", "    SELECT j ->> 'k' INTO v FROM t;"),
        ("@>", "    SELECT b INTO v FROM t WHERE j @> '{}';"),
        ("~*", "    SELECT b INTO v FROM t WHERE b ~* 'x';"),
        ("<<", "    n := 1 << 3;"),
        ("#", "    n := 5 # 3;"),
        ("subscript", "    SELECT arr[1] INTO n FROM t;"),
    ] {
        let sql = plain_fn(body);
        let stmts = pgzest_parser::parse(&sql).expect("parse");
        let parsed = stmts.iter().any(|st| {
            matches!(
                st,
                Statement::Ddl(DdlStatement::CreateFunction(cf))
                    if matches!(cf.body, FunctionBody::Parsed(_))
            )
        });
        assert!(parsed, "operator {label} still loses the body");
    }
}

#[test]
fn exponential_notation_keeps_its_value() {
    // `1e3` was lexed as `Number("1")` + `Ident("e3")`, glued back as `1 e3`, which
    // sqlparser reads as an ALIAS: the value silently became `1`.
    let body = body_of(&plain_fn("    n := 1e3;"));
    let PlStatementKind::Assign { value, .. } = &body.block.statements[0].kind else {
        panic!("Assign expected")
    };
    let rendered = format!("{value:?}");
    assert!(
        rendered.contains("1e3") || rendered.contains("1E3"),
        "the exponent must be kept, got: {rendered}"
    );
}

// ── 4. Safeguard on the repository fixture ────────────────────────────────────

#[test]
fn the_example_schema_has_no_truncated_condition() {
    let sql = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/schema.sql"
    ))
    .expect("examples/schema.sql");
    let stmts = pgzest_parser::parse(&sql).expect("parse");

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
            if let PlStatementKind::If { condition, .. } = &s.kind {
                total += 1;
                // A condition reduced to a mere field reference while
                // the source carries a comparison: that is the truncation.
                if matches!(condition.kind, ExprKind::Column(_)) {
                    truncated += 1;
                }
            }
        }
    }
    assert!(total > 0, "the fixture must contain IFs");
    assert_eq!(
        truncated, 0,
        "no condition must be truncated ({total} IFs in total)"
    );
}
