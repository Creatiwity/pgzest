//! Pipeline de parse d'un texte : découpage sur les tokens, grammaire de
//! pgweave ou de sqlparser pour chaque statement, conversion vers l'AST,
//! analyse des corps de routines.
//!
//! Aucune réécriture du texte : chaque position vient d'un token du texte
//! soumis, et la seule traduction est celle d'[`Offsets`] vers le document.

use pgweave_ast::Statement;
use pgweave_ast::ddl::{DdlStatement, FunctionBody, FunctionLanguage};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::Token;

use crate::convert;
use crate::dialect::PgweaveDialect;
use crate::error::ParseError;
use crate::grammar::Ctx;
use crate::grammar::routine::RoutineBody;
use crate::split::{LineIndex, StatementTokens, Unit};
use crate::{Offsets, ParseOutcome, SkipReason, SkippedStatement};

pub(crate) static DIALECT: PgweaveDialect = PgweaveDialect;

/// Parse `text`, situé à la position `base` du texte soumis au parse
/// englobant (0 pour un fichier ; le début du corps pour un corps de
/// routine). Les positions produites sont traduites par `offsets`.
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
    let mut p = Parser::new(&DIALECT).with_tokens_with_locations(st.tokens);

    let parsed = match crate::grammar::parse(&mut p, ctx) {
        Some(ours) => ours,
        None => {
            sqlparser_statement(&mut p).and_then(|s| Ok((convert::convert_statement(s)?, None)))
        }
    };

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

/// Un statement SQL à partir de ses tokens (sans le `;`), dans le contexte de
/// position déjà ouvert pour `ctx.text`. Sert aux statements SQL d'un corps
/// PL/pgSQL : ils sont lus exactement comme ceux d'un fichier. Le corps d'une
/// routine qui serait définie là n'est pas analysé.
pub(crate) fn statement_from_tokens(
    tokens: Vec<sqlparser::tokenizer::TokenWithSpan>,
    ctx: &Ctx,
) -> Result<Statement, ParseError> {
    let mut p = Parser::new(&DIALECT).with_tokens_with_locations(tokens);
    match crate::grammar::parse(&mut p, ctx) {
        Some(r) => r.map(|(s, _)| s),
        None => sqlparser_statement(&mut p).and_then(convert::convert_statement),
    }
}

/// Un statement par sqlparser, qui doit consommer tous les tokens.
fn sqlparser_statement(p: &mut Parser) -> Result<sqlparser::ast::Statement, ParseError> {
    let stmt = p
        .parse_statement()
        .map_err(|e| ParseError::Syntax(e.to_string()))?;
    if p.peek_token_ref().token != Token::EOF {
        let found = p.peek_token_ref().clone();
        return Err(ParseError::Syntax(
            p.expected_ref::<()>("fin du statement", &found)
                .unwrap_err()
                .to_string(),
        ));
    }
    Ok(stmt)
}

/// Analyse le corps d'une routine et l'installe dans `stmt`. Renvoie la
/// raison de l'échec si le corps n'a pas pu être analysé — il reste alors en
/// `Raw`, et l'appelant le signale.
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
        Statement::Utility(pgweave_ast::Utility {
            kind:
                pgweave_ast::UtilityKind::Do {
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
                // Langage externe : le corps est un symbole ou du code
                // étranger, il n'y a rien à analyser.
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
