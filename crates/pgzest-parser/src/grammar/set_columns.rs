//! `ON DELETE SET NULL (columns)` / `ON DELETE SET DEFAULT (columns)`
//! (PostgreSQL 15) on a foreign key, which sqlparser does not read.
//!
//! As for `RETURNING WITH`, the column lists are removed from the
//! **tokens** before parsing — the other tokens keep their positions — then
//! attached to the converted foreign keys. Not a text rewrite.

use pgzest_ast::Statement;
use pgzest_ast::ddl::{AlterTableOp, DdlStatement, ReferentialAction, TableConstraint};
use pgzest_ast::expr::Ident;
use sqlparser::tokenizer::{Token, TokenWithSpan};

use crate::convert::convert_ident;
use crate::error::ParseError;

fn is_word(t: &Token, w: &str) -> bool {
    matches!(t, Token::Word(word) if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(w))
}

/// The column list of each `ON DELETE SET NULL | SET DEFAULT` of a table
/// constraint, in text order; `None` where it has none.
pub(crate) struct SetColumns(Vec<Option<Vec<Ident>>>);

/// Removes the column lists, and returns them; `None` when the statement
/// has none (the usual case: nothing changes).
pub(crate) fn extract(tokens: &mut Vec<TokenWithSpan>) -> Result<Option<SetColumns>, ParseError> {
    let significant: Vec<usize> = (0..tokens.len())
        .filter(|&i| !matches!(tokens[i].token, Token::Whitespace(_)))
        .collect();
    let tok = |k: usize| significant.get(k).map(|&i| &tokens[i].token);

    // Start of the element (column, constraint, `ALTER TABLE` operation)
    // at each depth.
    let mut starts = vec![0usize];
    let mut found: Vec<Option<Vec<Ident>>> = Vec::new();
    // Token ranges (significant indexes) of the lists to remove.
    let mut remove: Vec<(usize, usize)> = Vec::new();
    let mut any_list = false;
    let mut k = 0;
    while k < significant.len() {
        let t = tok(k).expect("in range");
        match t {
            Token::LParen | Token::LBracket => starts.push(k + 1),
            Token::RParen | Token::RBracket => {
                if starts.len() > 1 {
                    starts.pop();
                }
            }
            Token::Comma => *starts.last_mut().expect("depth 0") = k + 1,
            t if is_word(t, "ON")
                && tok(k + 1).is_some_and(|t| is_word(t, "DELETE") || is_word(t, "UPDATE"))
                && tok(k + 2).is_some_and(|t| is_word(t, "SET"))
                && tok(k + 3).is_some_and(|t| is_word(t, "NULL") || is_word(t, "DEFAULT")) =>
            {
                let delete = tok(k + 1).is_some_and(|t| is_word(t, "DELETE"));
                let has_list = tok(k + 4) == Some(&Token::LParen);
                if has_list && !delete {
                    return Err(ParseError::Syntax(
                        "a column list with SET NULL or SET DEFAULT is only supported for ON \
                         DELETE actions"
                            .into(),
                    ));
                }
                if delete {
                    // A table constraint names `FOREIGN KEY` in its element;
                    // a column's `REFERENCES` does not.
                    let start = *starts.last().expect("depth 0");
                    let in_constraint =
                        (start..k).any(|j| tok(j).is_some_and(|t| is_word(t, "FOREIGN")));
                    if !in_constraint {
                        if has_list {
                            return Err(ParseError::Unsupported(
                                "ON DELETE SET NULL | SET DEFAULT with a column list in a column \
                                 constraint (write it as a table constraint, FOREIGN KEY (…))"
                                    .into(),
                            ));
                        }
                    } else if has_list {
                        any_list = true;
                        let mut columns = Vec::new();
                        let mut j = k + 5;
                        loop {
                            let Some(Token::Word(w)) = tok(j) else {
                                return Err(ParseError::Syntax(
                                    "ON DELETE SET … (: column name expected".into(),
                                ));
                            };
                            columns.push(convert_ident(
                                w.clone().into_ident(tokens[significant[j]].span),
                            ));
                            match tok(j + 1) {
                                Some(Token::Comma) => j += 2,
                                Some(Token::RParen) => break,
                                _ => {
                                    return Err(ParseError::Syntax(
                                        "ON DELETE SET … (: , or ) expected".into(),
                                    ));
                                }
                            }
                        }
                        remove.push((k + 4, j + 1));
                        found.push(Some(columns));
                        k = j + 2;
                        continue;
                    } else {
                        found.push(None);
                    }
                }
                k += 4;
                continue;
            }
            _ => {}
        }
        k += 1;
    }
    if !any_list {
        return Ok(None);
    }
    for &(from, to) in remove.iter().rev() {
        tokens.drain(significant[from]..=significant[to]);
    }
    Ok(Some(SetColumns(found)))
}

/// Attaches the lists to the foreign keys of the converted statement: its
/// table constraints with `ON DELETE SET NULL | SET DEFAULT`, in text order.
pub(crate) fn apply(stmt: &mut Statement, lists: SetColumns) -> Result<(), ParseError> {
    let constraints: Vec<&mut TableConstraint> = match stmt {
        Statement::Ddl(DdlStatement::CreateTable(ct)) => ct.constraints.iter_mut().collect(),
        Statement::Ddl(DdlStatement::AlterTable(at)) => at
            .operations
            .iter_mut()
            .filter_map(|op| match op {
                AlterTableOp::AddConstraint { constraint, .. } => Some(constraint),
                _ => None,
            })
            .collect(),
        _ => {
            return Err(ParseError::Syntax(
                "ON DELETE SET … (columns) outside CREATE TABLE or ALTER TABLE".into(),
            ));
        }
    };
    let mut targets: Vec<&mut Vec<Ident>> = constraints
        .into_iter()
        .filter_map(|c| match c {
            TableConstraint::ForeignKey {
                on_delete: Some(ReferentialAction::SetNull | ReferentialAction::SetDefault),
                on_delete_columns,
                ..
            } => Some(on_delete_columns),
            _ => None,
        })
        .collect();
    if targets.len() != lists.0.len() {
        return Err(ParseError::Syntax(format!(
            "ON DELETE SET … (columns): {} action(s) read, {} foreign key(s) converted",
            lists.0.len(),
            targets.len()
        )));
    }
    for (target, list) in targets.iter_mut().zip(lists.0) {
        if let Some(columns) = list {
            **target = columns;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use pgzest_ast::Statement;
    use pgzest_ast::ddl::DdlStatement;

    use crate::SkipReason;

    fn one(sql: &str) -> Result<Statement, String> {
        let out = crate::parse_with_outcome(sql).expect("parse");
        match (out.statements.as_slice(), out.skipped.as_slice()) {
            ([s], []) => Ok(s.clone()),
            ([], [s]) => Err(match &s.reason {
                SkipReason::Unparsable(m) | SkipReason::Unsupported(m) => m.clone(),
                other => format!("{other:?}"),
            }),
            other => panic!("{other:?}"),
        }
    }

    fn constraint(sql: &str) -> String {
        match one(sql).expect(sql) {
            Statement::Ddl(DdlStatement::CreateTable(ct)) => {
                pgzest_ast::emit::emit_table_constraint(&ct.constraints[0])
            }
            Statement::Ddl(DdlStatement::AlterTable(at)) => match &at.operations[0] {
                pgzest_ast::ddl::AlterTableOp::AddConstraint { constraint, .. } => {
                    pgzest_ast::emit::emit_table_constraint(constraint)
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn column_lists_are_read() {
        assert_eq!(
            constraint(
                "CREATE TABLE c (a INT, b INT, FOREIGN KEY (a, b) REFERENCES p (x, y) \
                 ON DELETE SET NULL (b) ON UPDATE CASCADE)"
            ),
            "FOREIGN KEY (a, b) REFERENCES p (x, y) ON DELETE SET NULL (b) ON UPDATE CASCADE"
        );
        // Several foreign keys, with and without a list: each keeps its own.
        let sql = "CREATE TABLE c (a INT, b INT, \
                   CONSTRAINT f1 FOREIGN KEY (a) REFERENCES p ON DELETE SET NULL, \
                   CONSTRAINT f2 FOREIGN KEY (a, b) REFERENCES q ON DELETE SET DEFAULT (a, b))";
        let Statement::Ddl(DdlStatement::CreateTable(ct)) = one(sql).unwrap() else {
            panic!()
        };
        let emitted: Vec<String> = ct
            .constraints
            .iter()
            .map(pgzest_ast::emit::emit_table_constraint)
            .collect();
        assert!(emitted[0].ends_with("ON DELETE SET NULL"), "{emitted:?}");
        assert!(
            emitted[1].ends_with("ON DELETE SET DEFAULT (a, b)"),
            "{emitted:?}"
        );
        assert_eq!(
            constraint(
                "ALTER TABLE c ADD CONSTRAINT f FOREIGN KEY (a, b) REFERENCES p \
                 ON DELETE SET NULL (\"B\")"
            ),
            "CONSTRAINT f FOREIGN KEY (a, b) REFERENCES p () ON DELETE SET NULL (\"B\")"
        );
    }

    #[test]
    fn misplaced_lists_are_refused() {
        let e = one("CREATE TABLE c (a INT, FOREIGN KEY (a) REFERENCES p ON UPDATE SET NULL (a))")
            .unwrap_err();
        assert!(e.contains("only supported for ON DELETE"), "{e}");
        let e = one("CREATE TABLE c (a INT REFERENCES p ON DELETE SET NULL (a))").unwrap_err();
        assert!(e.contains("column constraint"), "{e}");
    }
}
