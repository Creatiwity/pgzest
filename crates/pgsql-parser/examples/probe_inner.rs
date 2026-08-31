use pgsql_ast::{DdlStatement, Statement, ddl::FunctionBody, plpgsql::PlStatement};

fn main() {
    let sql = "CREATE TABLE users (id UUID PRIMARY KEY, email TEXT);\n\nCREATE OR REPLACE FUNCTION f() RETURNS INTEGER LANGUAGE plpgsql AS $$\nDECLARE\n    n INTEGER;\nBEGIN\n    SELECT count(*) INTO n FROM users WHERE nope = 1;\n    RETURN n;\nEND;\n$$;\n";
    println!(
        "offset attendu du SELECT interne = {:?}",
        sql.find("SELECT count")
    );
    println!("offset de 'nope' = {:?}", sql.find("nope"));
    for st in pgsql_parser::parse(sql).unwrap() {
        if let Statement::Ddl(DdlStatement::CreateFunction(cf)) = st {
            println!("body_start = {}", cf.body_start);
            if let FunctionBody::Parsed(b) = &cf.body {
                for s in &b.block.statements {
                    if let PlStatement::Sql(inner) = s {
                        let sp = match inner {
                            Statement::Select(x) => x.span,
                            _ => pgsql_ast::Span::DUMMY,
                        };
                        println!("  SQL interne span = {:?}", (sp.start, sp.end));
                        if (sp.start as usize) < sql.len() {
                            let e = (sp.end as usize).min(sql.len());
                            println!("    → {:?}", &sql[sp.start as usize..e]);
                        }
                    }
                }
            }
        }
    }
}
