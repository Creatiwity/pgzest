//! `CREATE [OR REPLACE] FUNCTION | PROCEDURE`, grammaire de PostgreSQL 18.
//!
//! sqlparser ne lit ni `CREATE PROCEDURE … AS $$…$$`, ni `COST`/`ROWS`, ni
//! `BEGIN ATOMIC` ; l'ancien parser réécrivait le texte (procédure déguisée
//! en fonction, options retirées puis recollées). Ici la grammaire est
//! écrite pour PostgreSQL, et seuls les éléments communs — noms, types,
//! expressions, statements des corps SQL standard — sont délégués à
//! sqlparser.

use pgweave_ast::Statement;
use pgweave_ast::ddl::{
    CreateFunction, CreateProcedure, DdlStatement, FunctionBody, FunctionLanguage, FunctionParam,
    FunctionSetting, Parallel, ParamMode, ReturnType, SettingValue, Volatility,
};
use pgweave_ast::expr::Ident;
use pgweave_ast::node::Span;
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

/// Corps d'une routine, avant analyse.
pub(crate) enum RoutineBody {
    /// `AS 'texte'` ou `AS $tag$texte$tag$` : le texte et la position de son
    /// premier caractère dans le texte en cours de parse.
    Text { content: String, start: usize },
    /// `BEGIN ATOMIC … END` : statements déjà parsés.
    Atomic(Vec<sp::Statement>),
    /// `RETURN expr`.
    Return(Box<sp::Expr>),
}

pub(crate) struct ParsedRoutine {
    /// Statement dont le corps vaut `FunctionBody::Raw("")` : le pipeline le
    /// remplace après analyse de [`ParsedRoutine::body`].
    pub statement: Statement,
    pub body: Option<RoutineBody>,
}

/// `CREATE [OR REPLACE] FUNCTION|PROCEDURE` en tête ?
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
        None => return Err(expected(p, "FUNCTION ou PROCEDURE")),
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
        // PostgreSQL : un corps SQL standard implique LANGUAGE sql ; sinon la
        // clause est obligatoire.
        None if matches!(
            opts.body,
            Some(RoutineBody::Atomic(_) | RoutineBody::Return(_))
        ) =>
        {
            FunctionLanguage::Sql
        }
        None => {
            return Err(ParseError::Unsupported(
                "routine sans clause LANGUAGE".to_string(),
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
            // Sans RETURNS : les paramètres OUT/INOUT définissent le résultat.
            None if params
                .iter()
                .any(|p| matches!(p.mode, ParamMode::Out | ParamMode::InOut)) =>
            {
                ReturnType::Table(
                    params
                        .iter()
                        .filter(|p| matches!(p.mode, ParamMode::Out | ParamMode::InOut))
                        .map(|p| {
                            (
                                p.name.clone().unwrap_or(Ident {
                                    value: "?column?".into(),
                                    quoted: false,
                                }),
                                p.data_type.clone(),
                            )
                        })
                        .collect(),
                )
            }
            None => {
                return Err(ParseError::Unsupported(
                    "fonction sans RETURNS ni paramètre OUT".to_string(),
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

// ── Paramètres ────────────────────────────────────────────────────────────────

struct RawParam {
    mode: ParamMode,
    name: Option<sp::Ident>,
    data_type: sp::DataType,
    default: Option<sp::Expr>,
}

/// `[mode] [nom] type [{DEFAULT | =} expr]`.
///
/// Le nom est optionnel, d'où une ambiguïté : `f(double precision)` est un
/// type seul, `f(p_id uuid)` un nom et un type. On essaie d'abord le type
/// seul, accepté seulement s'il est suivi de `,` `)` `DEFAULT` ou `=` ; sinon
/// nom puis type. C'est la règle effective de la grammaire de PostgreSQL.
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
        // `IN OUT` est un synonyme d'INOUT.
        Some(Keyword::IN) if p.parse_keyword(Keyword::OUT) => ParamMode::InOut,
        _ => ParamMode::In,
    };
    let type_only = p.maybe_parse(|p| {
        let t = p.parse_data_type()?;
        if ends_param(p) {
            Ok(t)
        } else {
            p.expected_ref("fin de paramètre", p.peek_token_ref())
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
        return Err(expected_at(p, "option de routine", &tok));
    };
    match w.keyword {
        Keyword::LANGUAGE => {
            p.next_token();
            let lang = match p.next_token().token {
                Token::Word(w) => w.value,
                Token::SingleQuotedString(s) => s,
                _ => return Err(expected_at(p, "nom de langage", &tok)),
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
                        return Err(expected(p, "DEFINER ou INVOKER"));
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
                _ => return Err(expected_at(p, "UNSAFE, RESTRICTED ou SAFE", &tok)),
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
            // Fonction de support du planificateur : sans effet sur l'analyse.
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
                _ => return Err(expected_at(p, "corps de routine", &body_tok)),
            };
            // `AS 'objet', 'symbole'` (langage C) : le second littéral est le
            // symbole, sans intérêt pour l'analyse.
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
        _ => return Err(expected_at(p, "option de routine", &tok)),
    }
    Ok(())
}

/// `nom { TO | = } { valeur [, …] | DEFAULT }` ou `nom FROM CURRENT`.
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
        return Err(expected(p, "TO ou ="));
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
            _ => return Err(expected_at(p, "valeur de paramètre", &tok)),
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

/// Erreur « attendu … » sur le prochain token.
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
