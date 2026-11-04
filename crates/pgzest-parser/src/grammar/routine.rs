//! `CREATE [OR REPLACE] FUNCTION | PROCEDURE`, PostgreSQL 18 grammar.
//!
//! sqlparser reads neither `CREATE PROCEDURE … AS $$…$$`, nor `COST`/`ROWS`, nor
//! `BEGIN ATOMIC`; the former parser rewrote the text (procedure disguised
//! as a function, options removed then glued back). Here the grammar is
//! written for PostgreSQL, and only the common elements — names, types,
//! expressions, statements of standard SQL bodies — are delegated to
//! sqlparser.

use pgzest_ast::Statement;
use pgzest_ast::ddl::{
    CreateFunction, CreateProcedure, DdlStatement, FunctionBody, FunctionLanguage, FunctionParam,
    FunctionSetting, Parallel, ParamMode, ReturnType, SettingValue, Volatility,
};
use pgzest_ast::node::Span;
use sqlparser::ast as sp;
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;

use super::{Ctx, keyword_at};
use crate::convert::{
    convert_data_type, convert_expr, convert_ident, convert_object_name,
    convert_return_type_from_dt,
};
use crate::error::ParseError;

/// Body of a routine, before analysis.
pub(crate) enum RoutineBody {
    /// `AS 'text'` or `AS $tag$text$tag$`: the text and the position of its
    /// first character in the text being parsed.
    Text { content: String, start: usize },
    /// `BEGIN ATOMIC … END`: already parsed statements.
    Atomic(Vec<sp::Statement>),
    /// `RETURN expr`.
    Return(Box<sp::Expr>),
}

pub(crate) struct ParsedRoutine {
    /// Statement whose body is `FunctionBody::Raw("")`: the pipeline
    /// replaces it after analyzing [`ParsedRoutine::body`].
    pub statement: Statement,
    pub body: Option<RoutineBody>,
}

/// `CREATE [OR REPLACE] FUNCTION|PROCEDURE` ahead?
pub(crate) fn is_routine(p: &Parser) -> bool {
    let k = |n| keyword_at(p, n);
    k(0) == Some(Keyword::CREATE)
        && match (k(1), k(2), k(3)) {
            (Some(Keyword::OR), Some(Keyword::REPLACE), Some(r)) | (Some(r), _, _) => {
                matches!(r, Keyword::FUNCTION | Keyword::PROCEDURE)
            }
            _ => false,
        }
}

pub(crate) fn parse_routine(p: &mut Parser, ctx: &Ctx) -> Result<ParsedRoutine, ParseError> {
    p.expect_keyword_is(Keyword::CREATE).map_err(pe)?;
    let or_replace = p.parse_keywords(&[Keyword::OR, Keyword::REPLACE]);
    let is_procedure = match p.parse_one_of_keywords(&[Keyword::FUNCTION, Keyword::PROCEDURE]) {
        Some(Keyword::PROCEDURE) => true,
        Some(_) => false,
        None => return Err(expected(p, "FUNCTION or PROCEDURE")),
    };
    let name = convert_object_name(p.parse_object_name(false).map_err(pe)?);

    p.expect_token(&Token::LParen).map_err(pe)?;
    let params = if p.consume_token(&Token::RParen) {
        Vec::new()
    } else {
        let params = p.parse_comma_separated(parse_param).map_err(pe)?;
        p.expect_token(&Token::RParen).map_err(pe)?;
        params
            .into_iter()
            .map(convert_param)
            .collect::<Result<Vec<_>, _>>()?
    };

    let mut return_type = None;
    if !is_procedure && p.parse_keyword(Keyword::RETURNS) {
        return_type = Some(parse_return_type(p)?);
    }

    let mut opts = Options::default();
    while p.peek_token_ref().token != Token::EOF {
        parse_option(p, ctx, &mut opts)?;
    }

    let language = match opts
        .language
        .as_deref()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("sql") => FunctionLanguage::Sql,
        Some("plpgsql") => FunctionLanguage::PlPgSql,
        Some("c") => FunctionLanguage::C,
        Some("internal") => FunctionLanguage::Internal,
        Some(other) => FunctionLanguage::Other(other.to_string()),
        // PostgreSQL: a standard SQL body implies LANGUAGE sql; otherwise the
        // clause is mandatory.
        None if matches!(
            opts.body,
            Some(RoutineBody::Atomic(_) | RoutineBody::Return(_))
        ) =>
        {
            FunctionLanguage::Sql
        }
        None => {
            return Err(ParseError::Unsupported(
                "routine without a LANGUAGE clause".to_string(),
            ));
        }
    };

    let body = FunctionBody::Raw(String::new());
    let statement = if is_procedure {
        Statement::Ddl(DdlStatement::CreateProcedure(CreateProcedure {
            id: crate::next_node_id(),
            span: Span::DUMMY,
            body_start: 0,
            name,
            or_replace,
            params,
            language,
            body,
            security_definer: opts.security_definer,
            settings: opts.settings,
        }))
    } else {
        let return_type = match return_type {
            Some(rt) => rt,
            // Without RETURNS: the OUT/INOUT parameters define the result.
            None if params
                .iter()
                .any(|p| matches!(p.mode, ParamMode::Out | ParamMode::InOut)) =>
            {
                ReturnType::OutParams
            }
            None => {
                return Err(ParseError::Unsupported(
                    "function without RETURNS or OUT parameter".to_string(),
                ));
            }
        };
        Statement::Ddl(DdlStatement::CreateFunction(CreateFunction {
            id: crate::next_node_id(),
            span: Span::DUMMY,
            body_start: 0,
            name,
            or_replace,
            params,
            return_type,
            language,
            body,
            security_definer: opts.security_definer,
            volatility: opts.volatility.unwrap_or(Volatility::Volatile),
            strict: opts.strict,
            leakproof: opts.leakproof,
            parallel: opts.parallel,
            cost: opts.cost,
            rows: opts.rows,
            settings: opts.settings,
        }))
    };
    Ok(ParsedRoutine {
        statement,
        body: opts.body,
    })
}

// ── DO ────────────────────────────────────────────────────────────────────────

pub(crate) fn is_do(p: &Parser) -> bool {
    keyword_at(p, 0) == Some(Keyword::DO)
}

/// `DO [LANGUAGE l] code` (the LANGUAGE clause may also follow the code):
/// anonymous block, PL/pgSQL by default.
pub(crate) fn parse_do(p: &mut Parser, ctx: &Ctx) -> Result<ParsedRoutine, ParseError> {
    p.expect_keyword_is(Keyword::DO).map_err(pe)?;
    let mut language = None;
    let mut body = None;
    while p.peek_token_ref().token != Token::EOF {
        if p.parse_keyword(Keyword::LANGUAGE) {
            language = Some(match p.next_token().token {
                Token::Word(w) => w.value,
                Token::SingleQuotedString(s) => s,
                _ => return Err(expected(p, "language name")),
            });
            continue;
        }
        let tok = p.next_token();
        let start = ctx.offset(tok.span.start);
        body = Some(match tok.token {
            Token::DollarQuotedString(d) => RoutineBody::Text {
                start: start + d.tag.as_deref().map_or(2, |t| t.len() + 2),
                content: d.value,
            },
            Token::SingleQuotedString(s) => RoutineBody::Text {
                content: s,
                start: start + 1,
            },
            _ => return Err(expected_at(p, "DO block body", &tok)),
        });
    }
    let Some(body) = body else {
        return Err(expected(p, "DO block body"));
    };
    let language = match language.as_deref().map(str::to_ascii_lowercase).as_deref() {
        None | Some("plpgsql") => FunctionLanguage::PlPgSql,
        Some("sql") => FunctionLanguage::Sql,
        Some(other) => FunctionLanguage::Other(other.to_string()),
    };
    Ok(ParsedRoutine {
        statement: Statement::Utility(pgzest_ast::Utility {
            span: Span::DUMMY,
            kind: pgzest_ast::UtilityKind::Do {
                language,
                body: FunctionBody::Raw(String::new()),
                body_start: 0,
            },
        }),
        body: Some(body),
    })
}

// ── Parameters ────────────────────────────────────────────────────────────────

struct RawParam {
    mode: ParamMode,
    name: Option<sp::Ident>,
    data_type: sp::DataType,
    default: Option<sp::Expr>,
}

/// `[mode] [name] type [{DEFAULT | =} expr]`.
///
/// The name is optional, hence an ambiguity: `f(double precision)` is a
/// type alone, `f(p_id uuid)` a name and a type. We first try the type
/// alone, accepted only if followed by `,` `)` `DEFAULT` or `=`; otherwise
/// name then type. This is the effective rule of PostgreSQL's grammar.
fn parse_param(p: &mut Parser) -> Result<RawParam, ParserError> {
    let mode = match p.parse_one_of_keywords(&[
        Keyword::IN,
        Keyword::OUT,
        Keyword::INOUT,
        Keyword::VARIADIC,
    ]) {
        Some(Keyword::OUT) => ParamMode::Out,
        Some(Keyword::INOUT) => ParamMode::InOut,
        Some(Keyword::VARIADIC) => ParamMode::Variadic,
        // `IN OUT` is a synonym for INOUT.
        Some(Keyword::IN) if p.parse_keyword(Keyword::OUT) => ParamMode::InOut,
        _ => ParamMode::In,
    };
    let type_only = p.maybe_parse(|p| {
        let t = p.parse_data_type()?;
        if ends_param(p) {
            Ok(t)
        } else {
            p.expected_ref("end of parameter", p.peek_token_ref())
        }
    })?;
    let (name, data_type) = match type_only {
        Some(t) => (None, t),
        None => {
            let n = p.parse_identifier()?;
            (Some(n), p.parse_data_type()?)
        }
    };
    let default = if p.parse_keyword(Keyword::DEFAULT) || p.consume_token(&Token::Eq) {
        Some(p.parse_expr()?)
    } else {
        None
    };
    Ok(RawParam {
        mode,
        name,
        data_type,
        default,
    })
}

fn ends_param(p: &Parser) -> bool {
    matches!(
        &p.peek_token_ref().token,
        Token::Comma | Token::RParen | Token::Eq
    ) || keyword_at(p, 0) == Some(Keyword::DEFAULT)
}

fn convert_param(raw: RawParam) -> Result<FunctionParam, ParseError> {
    Ok(FunctionParam {
        name: raw.name.map(convert_ident),
        data_type: convert_data_type(raw.data_type)?,
        mode: raw.mode,
        default: raw.default.map(convert_expr).transpose()?,
    })
}

// ── RETURNS ───────────────────────────────────────────────────────────────────

fn parse_return_type(p: &mut Parser) -> Result<ReturnType, ParseError> {
    if p.parse_keyword(Keyword::SETOF) {
        return Ok(ReturnType::SetOf(convert_data_type(
            p.parse_data_type().map_err(pe)?,
        )?));
    }
    if keyword_at(p, 0) == Some(Keyword::TABLE) && p.peek_nth_token_ref(1).token == Token::LParen {
        p.next_token();
        p.next_token();
        let cols = p
            .parse_comma_separated(|p| Ok((p.parse_identifier()?, p.parse_data_type()?)))
            .map_err(pe)?;
        p.expect_token(&Token::RParen).map_err(pe)?;
        return Ok(ReturnType::Table(
            cols.into_iter()
                .map(|(n, t)| Ok((convert_ident(n), convert_data_type(t)?)))
                .collect::<Result<_, ParseError>>()?,
        ));
    }
    let dt = p.parse_data_type().map_err(pe)?;
    match &dt {
        sp::DataType::Custom(name, _) if is_word(name, "void") => Ok(ReturnType::Void),
        _ => convert_return_type_from_dt(dt),
    }
}

fn is_word(name: &sp::ObjectName, word: &str) -> bool {
    matches!(name.0.as_slice(), [sp::ObjectNamePart::Identifier(id)] if id.value.eq_ignore_ascii_case(word))
}

// ── Options ───────────────────────────────────────────────────────────────────

#[derive(Default)]
struct Options {
    language: Option<String>,
    volatility: Option<Volatility>,
    strict: bool,
    leakproof: bool,
    security_definer: bool,
    parallel: Option<Parallel>,
    cost: Option<u64>,
    rows: Option<u64>,
    settings: Vec<FunctionSetting>,
    body: Option<RoutineBody>,
}

fn parse_option(p: &mut Parser, ctx: &Ctx, o: &mut Options) -> Result<(), ParseError> {
    let tok = p.peek_token_ref().clone();
    let Token::Word(w) = &tok.token else {
        return Err(expected_at(p, "routine option", &tok));
    };
    match w.keyword {
        Keyword::LANGUAGE => {
            p.next_token();
            let lang = match p.next_token().token {
                Token::Word(w) => w.value,
                Token::SingleQuotedString(s) => s,
                _ => return Err(expected_at(p, "language name", &tok)),
            };
            o.language = Some(lang);
        }
        Keyword::IMMUTABLE | Keyword::STABLE | Keyword::VOLATILE => {
            p.next_token();
            o.volatility = Some(match w.keyword {
                Keyword::IMMUTABLE => Volatility::Immutable,
                Keyword::STABLE => Volatility::Stable,
                _ => Volatility::Volatile,
            });
        }
        Keyword::STRICT => {
            p.next_token();
            o.strict = true;
        }
        Keyword::CALLED => {
            p.next_token();
            expect_keywords(p, &[Keyword::ON, Keyword::NULL, Keyword::INPUT])?;
            o.strict = false;
        }
        Keyword::RETURNS => {
            p.next_token();
            expect_keywords(
                p,
                &[Keyword::NULL, Keyword::ON, Keyword::NULL, Keyword::INPUT],
            )?;
            o.strict = true;
        }
        Keyword::NOT => {
            p.next_token();
            p.expect_keyword_is(Keyword::LEAKPROOF).map_err(pe)?;
            o.leakproof = false;
        }
        Keyword::LEAKPROOF => {
            p.next_token();
            o.leakproof = true;
        }
        Keyword::EXTERNAL | Keyword::SECURITY => {
            p.next_token();
            if w.keyword == Keyword::EXTERNAL {
                p.expect_keyword_is(Keyword::SECURITY).map_err(pe)?;
            }
            o.security_definer =
                match p.parse_one_of_keywords(&[Keyword::DEFINER, Keyword::INVOKER]) {
                    Some(Keyword::DEFINER) => true,
                    Some(_) => false,
                    None => {
                        return Err(expected(p, "DEFINER or INVOKER"));
                    }
                };
        }
        Keyword::PARALLEL => {
            p.next_token();
            o.parallel = Some(match p.next_token().token {
                Token::Word(w) if w.value.eq_ignore_ascii_case("unsafe") => Parallel::Unsafe,
                Token::Word(w) if w.value.eq_ignore_ascii_case("restricted") => {
                    Parallel::Restricted
                }
                Token::Word(w) if w.value.eq_ignore_ascii_case("safe") => Parallel::Safe,
                _ => return Err(expected_at(p, "UNSAFE, RESTRICTED or SAFE", &tok)),
            });
        }
        Keyword::COST | Keyword::ROWS => {
            p.next_token();
            let n = p.parse_literal_uint().map_err(pe)?;
            if w.keyword == Keyword::COST {
                o.cost = Some(n);
            } else {
                o.rows = Some(n);
            }
        }
        Keyword::SUPPORT => {
            // Planner support function: no effect on the analysis.
            p.next_token();
            p.parse_object_name(false).map_err(pe)?;
        }
        Keyword::WINDOW => {
            p.next_token();
        }
        Keyword::SET => {
            p.next_token();
            o.settings.push(parse_setting(p)?);
        }
        Keyword::AS => {
            p.next_token();
            let body_tok = p.next_token();
            let start = ctx.offset(body_tok.span.start);
            let (content, prefix_len) = match body_tok.token {
                Token::DollarQuotedString(d) => {
                    let delimiter = d.tag.as_deref().map_or(2, |t| t.len() + 2);
                    (d.value, delimiter)
                }
                Token::SingleQuotedString(s) => (s, 1),
                Token::EscapedStringLiteral(s) => (s, 2),
                _ => return Err(expected_at(p, "routine body", &body_tok)),
            };
            // `AS 'object', 'symbol'` (C language): the second literal is the
            // symbol, irrelevant to the analysis.
            if p.consume_token(&Token::Comma) {
                p.parse_literal_string().map_err(pe)?;
            }
            o.body = Some(RoutineBody::Text {
                content,
                start: start + prefix_len,
            });
        }
        Keyword::BEGIN if keyword_at(p, 1) == Some(Keyword::ATOMIC) => {
            p.next_token();
            p.next_token();
            let mut stmts = Vec::new();
            while !p.parse_keyword(Keyword::END) {
                stmts.push(p.parse_statement().map_err(pe)?);
                if !p.consume_token(&Token::SemiColon) {
                    p.expect_keyword_is(Keyword::END).map_err(pe)?;
                    break;
                }
            }
            o.body = Some(RoutineBody::Atomic(stmts));
        }
        Keyword::RETURN => {
            p.next_token();
            o.body = Some(RoutineBody::Return(Box::new(p.parse_expr().map_err(pe)?)));
        }
        _ if w.value.eq_ignore_ascii_case("transform") => {
            return Err(ParseError::Unsupported(
                "clause TRANSFORM FOR TYPE".to_string(),
            ));
        }
        _ => return Err(expected_at(p, "routine option", &tok)),
    }
    Ok(())
}

/// `name { TO | = } { value [, …] | DEFAULT }` or `name FROM CURRENT`.
fn parse_setting(p: &mut Parser) -> Result<FunctionSetting, ParseError> {
    let name = p
        .parse_object_name(false)
        .map_err(pe)?
        .0
        .iter()
        .map(|part| part.to_string())
        .collect::<Vec<_>>()
        .join(".");
    if p.parse_keywords(&[Keyword::FROM, Keyword::CURRENT]) {
        return Ok(FunctionSetting {
            name,
            value: SettingValue::FromCurrent,
        });
    }
    if !p.parse_keyword(Keyword::TO) && !p.consume_token(&Token::Eq) {
        return Err(expected(p, "TO or ="));
    }
    if p.parse_keyword(Keyword::DEFAULT) {
        return Ok(FunctionSetting {
            name,
            value: SettingValue::Values(Vec::new()),
        });
    }
    let mut values = Vec::new();
    loop {
        let tok = p.next_token();
        values.push(match tok.token {
            Token::Word(w) => w.value,
            Token::SingleQuotedString(s) => s,
            Token::Number(n, _) => n,
            _ => return Err(expected_at(p, "parameter value", &tok)),
        });
        if !p.consume_token(&Token::Comma) {
            break;
        }
    }
    Ok(FunctionSetting {
        name,
        value: SettingValue::Values(values),
    })
}

fn expect_keywords(p: &mut Parser, kws: &[Keyword]) -> Result<(), ParseError> {
    for k in kws {
        p.expect_keyword_is(*k).map_err(pe)?;
    }
    Ok(())
}

/// "Expected …" error on the next token.
fn expected(p: &Parser, what: &str) -> ParseError {
    let found = p.peek_token_ref().clone();
    expected_at(p, what, &found)
}

fn expected_at(p: &Parser, what: &str, found: &sqlparser::tokenizer::TokenWithSpan) -> ParseError {
    pe(p.expected_ref::<()>(what, found).unwrap_err())
}

fn pe(e: ParserError) -> ParseError {
    ParseError::Syntax(e.to_string())
}
