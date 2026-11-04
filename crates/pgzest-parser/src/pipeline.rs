//! Parse pipeline for a text: splitting on tokens, pgzest's or sqlparser's
//! grammar for each statement, conversion to the AST, analysis of
//! routine bodies.
//!
//! No text rewriting: every position comes from a token of the submitted
//! text, and the only translation is the one of [`Offsets`] into the document.

use pgzest_ast::Statement;
use pgzest_ast::ddl::{DdlStatement, FunctionBody, FunctionLanguage};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Token;

use crate::convert;
use crate::dialect::PgzestDialect;
use crate::error::ParseError;
use crate::grammar::Ctx;
use crate::grammar::routine::RoutineBody;
use crate::split::{LineIndex, StatementTokens, Unit};
use crate::{Offsets, ParseOutcome, SkipReason, SkippedStatement};

pub(crate) static DIALECT: PgzestDialect = PgzestDialect;

/// Parses `text`, located at position `base` of the text submitted to the
/// enclosing parse (0 for a file; the start of the body for a routine
/// body). The produced positions are translated by `offsets`.
pub(crate) fn parse_text(
    text: &str,
    base: usize,
    offsets: &Offsets,
) -> Result<ParseOutcome, ParseError> {
    let _ids = crate::ids::session();
    let _positions = convert::enter_span_ctx(text, base, offsets.clone());
    let index = LineIndex::new(text);
    let ctx = Ctx {
        text,
        index: &index,
    };

    let mut out = ParseOutcome::default();
    for unit in crate::split::split(text) {
        match unit {
            Unit::Unreadable {
                start,
                end,
                message,
            } => out.skipped.push(skipped(
                text,
                base,
                offsets,
                start,
                end,
                SkipReason::Unparsable(message),
            )),
            Unit::Statement(st) => parse_statement(st, &ctx, base, offsets, &mut out)?,
        }
    }
    Ok(out)
}

fn parse_statement(
    st: StatementTokens,
    ctx: &Ctx,
    base: usize,
    offsets: &Offsets,
    out: &mut ParseOutcome,
) -> Result<(), ParseError> {
    let (start, end) = (st.start, st.end);
    let mut tokens = st.tokens;
    let parsed = crate::grammar::returning::extract(&mut tokens).and_then(|aliases| {
        let mut p = Parser::new(&DIALECT).with_tokens_with_locations(tokens);
        let mut parsed = match crate::grammar::parse(&mut p, ctx) {
            Some(ours) => ours,
            None => {
                sqlparser_statement(&mut p).and_then(|s| Ok((convert::convert_statement(s)?, None)))
            }
        };
        if let (Some(a), Ok((stmt, _))) = (aliases, &mut parsed) {
            crate::grammar::returning::apply(stmt, a)?;
        }
        parsed
    });

    let (mut stmt, body) = match parsed {
        Ok(ok) => ok,
        Err(ParseError::Syntax(why)) => {
            out.skipped.push(skipped(
                ctx.text,
                base,
                offsets,
                start,
                end,
                SkipReason::Unparsable(why),
            ));
            return Ok(());
        }
        Err(ParseError::Unsupported(what)) => {
            out.skipped.push(skipped(
                ctx.text,
                base,
                offsets,
                start,
                end,
                SkipReason::Unsupported(what),
            ));
            return Ok(());
        }
    };

    let (span_start, span_end) = offsets.span(base + start, base + end);
    crate::set_stmt_span(&mut stmt, span_start, span_end);

    if let Some(body) = body
        && let Some(reason) = analyze_body(&mut stmt, body, base, offsets)?
    {
        let name = routine_name(&stmt);
        out.skipped.push(skipped(
            ctx.text,
            base,
            offsets,
            start,
            end,
            SkipReason::UnparsedBody { name, reason },
        ));
    }
    out.statements.push(stmt);
    Ok(())
}

/// A SQL statement from its tokens (without the `;`), in the position
/// context already opened for `ctx.text`. Used for the SQL statements of a
/// PL/pgSQL body: they are read exactly like those of a file. The body of a
/// routine that would be defined there is not analyzed.
pub(crate) fn statement_from_tokens(
    tokens: Vec<sqlparser::tokenizer::TokenWithSpan>,
    ctx: &Ctx,
) -> Result<Statement, ParseError> {
    let mut tokens = tokens;
    let aliases = crate::grammar::returning::extract(&mut tokens)?;
    let mut p = Parser::new(&DIALECT).with_tokens_with_locations(tokens);
    let mut stmt = match crate::grammar::parse(&mut p, ctx) {
        Some(r) => r.map(|(s, _)| s),
        None => sqlparser_statement(&mut p).and_then(convert::convert_statement),
    }?;
    if let Some(a) = aliases {
        crate::grammar::returning::apply(&mut stmt, a)?;
    }
    Ok(stmt)
}

/// A statement via sqlparser, which must consume all tokens.
fn sqlparser_statement(p: &mut Parser) -> Result<sqlparser::ast::Statement, ParseError> {
    let stmt = p
        .parse_statement()
        .map_err(|e| ParseError::Syntax(e.to_string()))?;
    if p.peek_token_ref().token != Token::EOF {
        let found = p.peek_token_ref().clone();
        return Err(ParseError::Syntax(
            p.expected_ref::<()>("end of statement", &found)
                .unwrap_err()
                .to_string(),
        ));
    }
    Ok(stmt)
}

/// Analyzes a routine's body and installs it in `stmt`. Returns the
/// reason for failure if the body could not be analyzed — it then stays
/// `Raw`, and the caller reports it.
fn analyze_body(
    stmt: &mut Statement,
    body: RoutineBody,
    base: usize,
    offsets: &Offsets,
) -> Result<Option<String>, ParseError> {
    let (language, slot, body_start) = match stmt {
        Statement::Ddl(DdlStatement::CreateFunction(f)) => {
            (&f.language, &mut f.body, &mut f.body_start)
        }
        Statement::Ddl(DdlStatement::CreateProcedure(p)) => {
            (&p.language, &mut p.body, &mut p.body_start)
        }
        Statement::Utility(pgzest_ast::Utility {
            kind:
                pgzest_ast::UtilityKind::Do {
                    language,
                    body,
                    body_start,
                },
            ..
        }) => (&*language, body, body_start),
        _ => return Ok(None),
    };

    let (analyzed, failure) = match body {
        RoutineBody::Text { content, start } => {
            *body_start = offsets.point(base + start) as u32;
            match language {
                FunctionLanguage::PlPgSql => {
                    match crate::plpgsql::parse_plpgsql_body_mapped(&content, base + start, offsets)
                    {
                        Ok(parsed) => (FunctionBody::Parsed(Box::new(parsed)), None),
                        Err(e) => (FunctionBody::Raw(content), Some(e.to_string())),
                    }
                }
                FunctionLanguage::Sql => {
                    let inner = parse_text(&content, base + start, offsets)?;
                    match inner.skipped.first() {
                        Some(sk) => (FunctionBody::Raw(content), Some(describe(&sk.reason))),
                        None => (sql_body(inner.statements), None),
                    }
                }
                // External language: the body is a symbol or foreign
                // code, there is nothing to analyze.
                _ => (FunctionBody::Raw(content), None),
            }
        }
        RoutineBody::Atomic(stmts) => {
            let converted: Result<Vec<_>, _> =
                stmts.into_iter().map(convert::convert_statement).collect();
            match converted {
                Ok(stmts) => (FunctionBody::Sql(stmts), None),
                Err(e) => (FunctionBody::Raw(String::new()), Some(e.to_string())),
            }
        }
        RoutineBody::Return(expr) => match convert::convert_expr(*expr) {
            Ok(e) => (FunctionBody::SqlReturn(e), None),
            Err(e) => (FunctionBody::Raw(String::new()), Some(e.to_string())),
        },
    };
    *slot = analyzed;
    Ok(failure)
}

fn sql_body(mut stmts: Vec<Statement>) -> FunctionBody {
    if stmts.len() == 1 && matches!(stmts[0], Statement::Select(_)) {
        let Some(Statement::Select(s)) = stmts.pop() else {
            unreachable!()
        };
        FunctionBody::SqlQuery(Box::new(s))
    } else {
        FunctionBody::Sql(stmts)
    }
}

fn describe(reason: &SkipReason) -> String {
    match reason {
        SkipReason::Unparsable(why) => why.clone(),
        SkipReason::Unsupported(what) => what.clone(),
        SkipReason::UnparsedBody { reason, .. } => reason.clone(),
    }
}

fn routine_name(stmt: &Statement) -> String {
    match stmt {
        Statement::Ddl(DdlStatement::CreateFunction(f)) => crate::object_name_last(&f.name),
        Statement::Ddl(DdlStatement::CreateProcedure(p)) => crate::object_name_last(&p.name),
        Statement::Utility(_) => "DO".into(),
        _ => "?".into(),
    }
}

fn skipped(
    text: &str,
    base: usize,
    offsets: &Offsets,
    start: usize,
    end: usize,
    reason: SkipReason,
) -> SkippedStatement {
    let (s, e) = offsets.span(base + start, base + end);
    SkippedStatement {
        sql: text[start..end].to_string(),
        start: s,
        end: e,
        reason,
    }
}
