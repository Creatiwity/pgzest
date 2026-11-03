//! Positions of table names: `FROM` references, and the targets of `INSERT`,
//! `UPDATE` and `DELETE`, in a PL/pgSQL body located inside its document.
//! Rewriting a body from its analysis (isolated temp tables) relies on them.

use pgweave_ast::plpgsql::{PlStatement, PlStatementKind};
use pgweave_ast::stmt::Statement;
use pgweave_parser::plpgsql::parse_plpgsql_body_at;

const PREFIX: &str = "CREATE FUNCTION f() RETURNS void LANGUAGE plpgsql AS $$";

fn statements(body: &str) -> (String, Vec<PlStatement>) {
    let doc = format!("{PREFIX}{body}$$;");
    let parsed = parse_plpgsql_body_at(body, PREFIX.len()).expect("body parses");
    (doc, parsed.block.statements)
}

fn sql(s: &PlStatement) -> &Statement {
    match &s.kind {
        PlStatementKind::Sql { statement, .. } => statement,
        other => panic!("not a SQL statement: {other:?}"),
    }
}

fn text(doc: &str, span: pgweave_ast::Span) -> &str {
    &doc[span.start as usize..span.end as usize]
}

#[test]
fn target_names_carry_their_position() {
    let (doc, stmts) = statements(
        "\nBEGIN\n  INSERT INTO pg_temp.work (a) VALUES (1);\n  \
         UPDATE work AS w SET a = 2 WHERE w.a = 1;\n  DELETE FROM work WHERE a = 2;\nEND",
    );
    let Statement::Insert(ins) = sql(&stmts[0]) else {
        panic!("INSERT expected")
    };
    assert_eq!(text(&doc, ins.table_span), "pg_temp.work");
    let Statement::Update(upd) = sql(&stmts[1]) else {
        panic!("UPDATE expected")
    };
    assert_eq!(text(&doc, upd.table.span), "work");
    let Statement::Delete(del) = sql(&stmts[2]) else {
        panic!("DELETE expected")
    };
    assert_eq!(text(&doc, del.table.span), "work");
}

#[test]
fn from_references_carry_their_position() {
    let (doc, stmts) = statements(
        "\nBEGIN\n  PERFORM 1 FROM orders o JOIN work w ON w.id = o.id \
         WHERE EXISTS (SELECT 1 FROM work);\nEND",
    );
    let PlStatementKind::Perform(select) = &stmts[0].kind else {
        panic!("PERFORM expected")
    };
    assert_eq!(text(&doc, select.from[0].span), "orders");
    assert_eq!(text(&doc, select.joins[0].table.span), "work");
}
