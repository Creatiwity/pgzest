//! DDL que sqlparser ne lit pas, ou que son AST ne sait pas représenter :
//! contraintes `EXCLUDE`, tables `UNLOGGED`, `ALTER TABLE … SET SCHEMA`,
//! `CREATE DOMAIN` complet, `CREATE/ALTER EXTENSION`, options PostgreSQL de
//! `CREATE SEQUENCE`, `ALTER TYPE … SET SCHEMA`.
//!
//! Les éléments communs (colonnes, contraintes ordinaires, opérations
//! d'`ALTER TABLE`, types, expressions) restent lus par sqlparser.

use pgweave_ast::Statement;
use pgweave_ast::ddl::{
    AlterExtension, AlterExtensionOp, AlterTable, AlterTableOp, AlterType, AlterTypeOp,
    CreateDomain, CreateExtension, CreateSequence, CreateTable, DdlStatement, DomainCheck,
    ExclusionElement, IndexElement, LikeFeature, LikeOption, OnCommit, TableConstraint, TableLike,
};
use pgweave_ast::expr::ExprKind;
use pgweave_ast::node::Span;
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::Token;

use super::keyword_at;
use crate::convert::{
    convert_alter_table_op, convert_column_def, convert_data_type, convert_expr, convert_ident,
    convert_object_name, convert_table_constraint,
};
use crate::error::ParseError;

type Result<T> = std::result::Result<T, ParseError>;

// ── CREATE TABLE ──────────────────────────────────────────────────────────────

/// `CREATE [GLOBAL|LOCAL] [TEMP|TEMPORARY|UNLOGGED] TABLE [IF NOT EXISTS] nom (`
/// — la forme à éléments. `CREATE TABLE … AS`, `PARTITION OF` et `OF type`
/// restent à sqlparser (et sont écartés s'il ne les représente pas).
pub(crate) fn is_create_table(p: &Parser) -> bool {
    let mut n = 1;
    if keyword_at(p, 0) != Some(Keyword::CREATE) {
        return false;
    }
    while matches!(
        keyword_at(p, n),
        Some(
            Keyword::GLOBAL
                | Keyword::LOCAL
                | Keyword::TEMP
                | Keyword::TEMPORARY
                | Keyword::UNLOGGED
        )
    ) {
        n += 1;
    }
    if keyword_at(p, n) != Some(Keyword::TABLE) {
        return false;
    }
    n += 1;
    if keyword_at(p, n) == Some(Keyword::IF) {
        n += 3;
    }
    // Nom éventuellement qualifié, puis `(`.
    n += 1;
    while p.peek_nth_token_ref(n).token == Token::Period {
        n += 2;
    }
    p.peek_nth_token_ref(n).token == Token::LParen
}

pub(crate) fn parse_create_table(p: &mut Parser) -> Result<Statement> {
    p.expect_keyword_is(Keyword::CREATE).map_err(pe)?;
    let (mut temporary, mut unlogged) = (false, false);
    while let Some(k) = p.parse_one_of_keywords(&[
        Keyword::GLOBAL,
        Keyword::LOCAL,
        Keyword::TEMP,
        Keyword::TEMPORARY,
        Keyword::UNLOGGED,
    ]) {
        match k {
            Keyword::TEMP | Keyword::TEMPORARY => temporary = true,
            Keyword::UNLOGGED => unlogged = true,
            // GLOBAL / LOCAL : sans effet dans PostgreSQL, conservés pour la
            // compatibilité SQL.
            _ => {}
        }
    }
    p.expect_keyword_is(Keyword::TABLE).map_err(pe)?;
    let if_not_exists = p.parse_keywords(&[Keyword::IF, Keyword::NOT, Keyword::EXISTS]);
    let name = convert_object_name(p.parse_object_name(false).map_err(pe)?);

    p.expect_token(&Token::LParen).map_err(pe)?;
    let mut columns = Vec::new();
    let mut constraints = Vec::new();
    let mut likes = Vec::new();
    if !p.consume_token(&Token::RParen) {
        loop {
            if is_exclude(p) {
                constraints.push(parse_exclude(p)?);
            } else if let Some(c) = p.parse_optional_table_constraint().map_err(pe)? {
                constraints.push(convert_table_constraint(c)?);
            } else if p.parse_keyword(Keyword::LIKE) {
                likes.push(parse_like(p, columns.len())?);
            } else {
                columns.push(convert_column_def(p.parse_column_def().map_err(pe)?)?);
            }
            if p.consume_token(&Token::RParen) {
                break;
            }
            p.expect_token(&Token::Comma).map_err(pe)?;
        }
    }

    let mut on_commit = None;
    while p.peek_token_ref().token != Token::EOF {
        if p.parse_keywords(&[Keyword::ON, Keyword::COMMIT]) {
            on_commit = Some(if p.parse_keywords(&[Keyword::PRESERVE, Keyword::ROWS]) {
                OnCommit::PreserveRows
            } else if p.parse_keywords(&[Keyword::DELETE, Keyword::ROWS]) {
                OnCommit::DeleteRows
            } else if p.parse_keyword(Keyword::DROP) {
                OnCommit::Drop
            } else {
                return Err(expected(p, "PRESERVE ROWS, DELETE ROWS ou DROP"));
            });
        } else if p.parse_keyword(Keyword::TABLESPACE) {
            // Emplacement physique : sans effet sur l'analyse.
            p.parse_identifier().map_err(pe)?;
        } else {
            let what = p.peek_token_ref().to_string();
            return Err(ParseError::Unsupported(format!(
                "option de CREATE TABLE non modélisée : {what}"
            )));
        }
    }

    Ok(Statement::Ddl(DdlStatement::CreateTable(CreateTable {
        id: crate::next_node_id(),
        span: Span::DUMMY,
        name,
        if_not_exists,
        temporary,
        unlogged,
        on_commit,
        columns,
        constraints,
        likes,
    })))
}

/// `LIKE source [{INCLUDING | EXCLUDING} option …]`, `LIKE` déjà lu.
fn parse_like(p: &mut Parser, position: usize) -> Result<TableLike> {
    let source = convert_object_name(p.parse_object_name(false).map_err(pe)?);
    let mut options = Vec::new();
    while let Some(k) = p.parse_one_of_keywords(&[Keyword::INCLUDING, Keyword::EXCLUDING]) {
        let feature = match &p.next_token().token {
            Token::Word(w) => match w.value.to_ascii_uppercase().as_str() {
                "COMMENTS" => LikeFeature::Comments,
                "COMPRESSION" => LikeFeature::Compression,
                "CONSTRAINTS" => LikeFeature::Constraints,
                "DEFAULTS" => LikeFeature::Defaults,
                "GENERATED" => LikeFeature::Generated,
                "IDENTITY" => LikeFeature::Identity,
                "INDEXES" => LikeFeature::Indexes,
                "STATISTICS" => LikeFeature::Statistics,
                "STORAGE" => LikeFeature::Storage,
                "ALL" => LikeFeature::All,
                _ => return Err(expected(p, "une option de LIKE")),
            },
            _ => return Err(expected(p, "une option de LIKE")),
        };
        options.push(LikeOption {
            including: k == Keyword::INCLUDING,
            feature,
        });
    }
    Ok(TableLike {
        source,
        position,
        options,
    })
}

/// `[CONSTRAINT nom] EXCLUDE` en tête ?
fn is_exclude(p: &Parser) -> bool {
    let is_exclude_at = |n| matches!(&p.peek_nth_token_ref(n).token, sqlparser::tokenizer::Token::Word(w) if w.value.eq_ignore_ascii_case("exclude"));
    is_exclude_at(0) || (keyword_at(p, 0) == Some(Keyword::CONSTRAINT) && is_exclude_at(2))
}

/// `[CONSTRAINT nom] EXCLUDE [USING méthode] (élément WITH opérateur, …)
/// [INCLUDE (…)] [WITH (…)] [USING INDEX TABLESPACE t] [WHERE (prédicat)]`.
fn parse_exclude(p: &mut Parser) -> Result<TableConstraint> {
    let name = if p.parse_keyword(Keyword::CONSTRAINT) {
        Some(convert_ident(p.parse_identifier().map_err(pe)?))
    } else {
        None
    };
    p.next_token(); // EXCLUDE
    let using = if p.parse_keyword(Keyword::USING) {
        Some(p.parse_identifier().map_err(pe)?.value.to_lowercase())
    } else {
        None
    };
    p.expect_token(&Token::LParen).map_err(pe)?;
    let mut elements = Vec::new();
    loop {
        let expr = convert_expr(p.parse_expr().map_err(pe)?)?;
        let element = match expr.kind {
            ExprKind::Column(c) if c.table.is_none() => IndexElement::Column(c.column),
            _ => IndexElement::Expr(expr),
        };
        p.expect_keyword_is(Keyword::WITH).map_err(pe)?;
        let mut operator = String::new();
        while !matches!(
            p.peek_token_ref().token,
            Token::Comma | Token::RParen | Token::EOF
        ) {
            operator.push_str(&p.next_token().token.to_string());
        }
        if operator.is_empty() {
            return Err(expected(p, "opérateur d'exclusion"));
        }
        elements.push(ExclusionElement { element, operator });
        if p.consume_token(&Token::RParen) {
            break;
        }
        p.expect_token(&Token::Comma).map_err(pe)?;
    }
    if p.parse_keyword(Keyword::INCLUDE) {
        p.parse_parenthesized_column_list(sqlparser::parser::IsOptional::Mandatory, false)
            .map_err(pe)?;
    }
    let where_clause = if p.parse_keyword(Keyword::WHERE) {
        p.expect_token(&Token::LParen).map_err(pe)?;
        let e = convert_expr(p.parse_expr().map_err(pe)?)?;
        p.expect_token(&Token::RParen).map_err(pe)?;
        Some(e)
    } else {
        None
    };
    Ok(TableConstraint::Exclude {
        name,
        using,
        elements,
        where_clause,
    })
}

// ── ALTER TABLE ───────────────────────────────────────────────────────────────

pub(crate) fn is_alter_table(p: &Parser) -> bool {
    keyword_at(p, 0) == Some(Keyword::ALTER) && keyword_at(p, 1) == Some(Keyword::TABLE)
}

/// `ALTER TABLE [IF EXISTS] [ONLY] nom opération, …` : les opérations
/// ordinaires sont lues par sqlparser, `ADD … EXCLUDE` et `SET SCHEMA` ici.
pub(crate) fn parse_alter_table(p: &mut Parser) -> Result<Statement> {
    p.expect_keyword_is(Keyword::ALTER).map_err(pe)?;
    p.expect_keyword_is(Keyword::TABLE).map_err(pe)?;
    let if_exists = p.parse_keywords(&[Keyword::IF, Keyword::EXISTS]);
    let _only = p.parse_keyword(Keyword::ONLY);
    let name = convert_object_name(p.parse_object_name(false).map_err(pe)?);
    let mut operations = Vec::new();
    loop {
        let op = if keyword_at(p, 0) == Some(Keyword::ADD)
            && {
                // ADD [CONSTRAINT nom] EXCLUDE
                let at = |n| matches!(&p.peek_nth_token_ref(n).token, Token::Word(w) if w.value.eq_ignore_ascii_case("exclude"));
                at(1) || (keyword_at(p, 1) == Some(Keyword::CONSTRAINT) && at(3))
            } {
            p.next_token();
            AlterTableOp::AddConstraint(parse_exclude(p)?)
        } else if p.parse_keywords(&[Keyword::SET, Keyword::SCHEMA]) {
            AlterTableOp::SetSchema(convert_ident(p.parse_identifier().map_err(pe)?))
        } else {
            convert_alter_table_op(p.parse_alter_table_operation().map_err(pe)?)?
        };
        operations.push(op);
        if !p.consume_token(&Token::Comma) {
            break;
        }
    }
    expect_end(p)?;
    Ok(Statement::Ddl(DdlStatement::AlterTable(AlterTable {
        name,
        if_exists,
        operations,
    })))
}

// ── CREATE DOMAIN ─────────────────────────────────────────────────────────────

pub(crate) fn is_create_domain(p: &Parser) -> bool {
    keyword_at(p, 0) == Some(Keyword::CREATE) && keyword_at(p, 1) == Some(Keyword::DOMAIN)
}

/// `CREATE DOMAIN nom [AS] type [COLLATE c] [DEFAULT expr]
/// [[CONSTRAINT nom] { NOT NULL | NULL | CHECK (expr) }] …`
pub(crate) fn parse_create_domain(p: &mut Parser) -> Result<Statement> {
    p.expect_keyword_is(Keyword::CREATE).map_err(pe)?;
    p.expect_keyword_is(Keyword::DOMAIN).map_err(pe)?;
    let name = convert_object_name(p.parse_object_name(false).map_err(pe)?);
    let _as = p.parse_keyword(Keyword::AS);
    let data_type = convert_data_type(p.parse_data_type().map_err(pe)?)?;
    let mut domain = CreateDomain {
        name,
        data_type,
        not_null: false,
        default: None,
        checks: Vec::new(),
    };
    while p.peek_token_ref().token != Token::EOF {
        if p.parse_keyword(Keyword::COLLATE) {
            p.parse_object_name(false).map_err(pe)?;
            continue;
        }
        if p.parse_keyword(Keyword::DEFAULT) {
            domain.default = Some(convert_expr(p.parse_expr().map_err(pe)?)?);
            continue;
        }
        let constraint_name = if p.parse_keyword(Keyword::CONSTRAINT) {
            Some(convert_ident(p.parse_identifier().map_err(pe)?))
        } else {
            None
        };
        if p.parse_keywords(&[Keyword::NOT, Keyword::NULL]) {
            domain.not_null = true;
        } else if p.parse_keyword(Keyword::NULL) {
            domain.not_null = false;
        } else if p.parse_keyword(Keyword::CHECK) {
            p.expect_token(&Token::LParen).map_err(pe)?;
            let expr = convert_expr(p.parse_expr().map_err(pe)?)?;
            p.expect_token(&Token::RParen).map_err(pe)?;
            domain.checks.push(DomainCheck {
                name: constraint_name,
                expr,
            });
        } else {
            return Err(expected(p, "contrainte de domaine"));
        }
    }
    Ok(Statement::Ddl(DdlStatement::CreateDomain(domain)))
}

// ── Extensions ────────────────────────────────────────────────────────────────

pub(crate) fn is_create_extension(p: &Parser) -> bool {
    keyword_at(p, 0) == Some(Keyword::CREATE) && keyword_at(p, 1) == Some(Keyword::EXTENSION)
}

/// `CREATE EXTENSION [IF NOT EXISTS] nom [WITH] [SCHEMA s] [VERSION v] [CASCADE]`
pub(crate) fn parse_create_extension(p: &mut Parser) -> Result<Statement> {
    p.expect_keyword_is(Keyword::CREATE).map_err(pe)?;
    p.expect_keyword_is(Keyword::EXTENSION).map_err(pe)?;
    let if_not_exists = p.parse_keywords(&[Keyword::IF, Keyword::NOT, Keyword::EXISTS]);
    let name = convert_ident(p.parse_identifier().map_err(pe)?);
    let _with = p.parse_keyword(Keyword::WITH);
    let mut ext = CreateExtension {
        name,
        if_not_exists,
        schema: None,
        version: None,
        cascade: false,
    };
    while p.peek_token_ref().token != Token::EOF {
        if p.parse_keyword(Keyword::SCHEMA) {
            ext.schema = Some(convert_ident(p.parse_identifier().map_err(pe)?));
        } else if p.parse_keyword(Keyword::VERSION) {
            ext.version = Some(word_or_string(p)?);
        } else if p.parse_keyword(Keyword::CASCADE) {
            ext.cascade = true;
        } else {
            return Err(expected(p, "SCHEMA, VERSION ou CASCADE"));
        }
    }
    Ok(Statement::Ddl(DdlStatement::CreateExtension(ext)))
}

pub(crate) fn is_alter_extension(p: &Parser) -> bool {
    keyword_at(p, 0) == Some(Keyword::ALTER) && keyword_at(p, 1) == Some(Keyword::EXTENSION)
}

/// `ALTER EXTENSION nom UPDATE [TO v]` ou `ALTER EXTENSION nom SET SCHEMA s`.
pub(crate) fn parse_alter_extension(p: &mut Parser) -> Result<Statement> {
    p.expect_keyword_is(Keyword::ALTER).map_err(pe)?;
    p.expect_keyword_is(Keyword::EXTENSION).map_err(pe)?;
    let name = convert_ident(p.parse_identifier().map_err(pe)?);
    let operation = if p.parse_keyword(Keyword::UPDATE) {
        if p.parse_keyword(Keyword::TO) {
            AlterExtensionOp::Update(Some(word_or_string(p)?))
        } else {
            AlterExtensionOp::Update(None)
        }
    } else if p.parse_keywords(&[Keyword::SET, Keyword::SCHEMA]) {
        AlterExtensionOp::SetSchema(convert_ident(p.parse_identifier().map_err(pe)?))
    } else {
        return Err(ParseError::Unsupported(
            "ALTER EXTENSION ADD/DROP d'un membre".to_string(),
        ));
    };
    expect_end(p)?;
    Ok(Statement::Ddl(DdlStatement::AlterExtension(
        AlterExtension { name, operation },
    )))
}

// ── CREATE SEQUENCE ───────────────────────────────────────────────────────────

pub(crate) fn is_create_sequence(p: &Parser) -> bool {
    let k = |n| keyword_at(p, n);
    k(0) == Some(Keyword::CREATE)
        && (k(1) == Some(Keyword::SEQUENCE)
            || (matches!(
                k(1),
                Some(Keyword::TEMP | Keyword::TEMPORARY | Keyword::UNLOGGED)
            ) && k(2) == Some(Keyword::SEQUENCE)))
}

/// `CREATE [TEMP] SEQUENCE [IF NOT EXISTS] nom [AS type] [INCREMENT [BY] n]
/// [MINVALUE n | NO MINVALUE] [MAXVALUE n | NO MAXVALUE] [START [WITH] n]
/// [CACHE n] [[NO] CYCLE] [OWNED BY {table.colonne | NONE}]`
pub(crate) fn parse_create_sequence(p: &mut Parser) -> Result<Statement> {
    p.expect_keyword_is(Keyword::CREATE).map_err(pe)?;
    let temporary = matches!(
        p.parse_one_of_keywords(&[Keyword::TEMP, Keyword::TEMPORARY, Keyword::UNLOGGED]),
        Some(Keyword::TEMP | Keyword::TEMPORARY)
    );
    p.expect_keyword_is(Keyword::SEQUENCE).map_err(pe)?;
    let if_not_exists = p.parse_keywords(&[Keyword::IF, Keyword::NOT, Keyword::EXISTS]);
    let mut seq = CreateSequence {
        name: convert_object_name(p.parse_object_name(false).map_err(pe)?),
        if_not_exists,
        temporary,
        data_type: None,
        increment: None,
        min_value: None,
        max_value: None,
        start: None,
        cache: None,
        cycle: false,
        owned_by: None,
    };
    while p.peek_token_ref().token != Token::EOF {
        if p.parse_keyword(Keyword::AS) {
            seq.data_type = Some(convert_data_type(p.parse_data_type().map_err(pe)?)?);
        } else if p.parse_keyword(Keyword::INCREMENT) {
            let _by = p.parse_keyword(Keyword::BY);
            seq.increment = Some(signed_integer(p)?);
        } else if p.parse_keyword(Keyword::MINVALUE) {
            seq.min_value = Some(signed_integer(p)?);
        } else if p.parse_keyword(Keyword::MAXVALUE) {
            seq.max_value = Some(signed_integer(p)?);
        } else if p.parse_keywords(&[Keyword::NO, Keyword::MINVALUE]) {
            seq.min_value = None;
        } else if p.parse_keywords(&[Keyword::NO, Keyword::MAXVALUE]) {
            seq.max_value = None;
        } else if p.parse_keywords(&[Keyword::NO, Keyword::CYCLE]) {
            seq.cycle = false;
        } else if p.parse_keyword(Keyword::START) {
            let _with = p.parse_keyword(Keyword::WITH);
            seq.start = Some(signed_integer(p)?);
        } else if p.parse_keyword(Keyword::CACHE) {
            seq.cache = Some(signed_integer(p)?);
        } else if p.parse_keyword(Keyword::CYCLE) {
            seq.cycle = true;
        } else if p.parse_keywords(&[Keyword::OWNED, Keyword::BY]) {
            seq.owned_by = if p.parse_keyword(Keyword::NONE) {
                None
            } else {
                Some(convert_object_name(p.parse_object_name(false).map_err(pe)?))
            };
        } else {
            return Err(expected(p, "option de séquence"));
        }
    }
    Ok(Statement::Ddl(DdlStatement::CreateSequence(seq)))
}

// ── ALTER TYPE … SET SCHEMA ───────────────────────────────────────────────────

/// Seule forme d'`ALTER TYPE` que sqlparser ne lit pas ; les autres
/// (`ADD VALUE`, `RENAME`) lui sont laissées.
pub(crate) fn is_alter_type_set_schema(p: &Parser) -> bool {
    if keyword_at(p, 0) != Some(Keyword::ALTER) || keyword_at(p, 1) != Some(Keyword::TYPE) {
        return false;
    }
    let mut n = 3;
    while p.peek_nth_token_ref(n).token == Token::Period {
        n += 2;
    }
    keyword_at(p, n) == Some(Keyword::SET) && keyword_at(p, n + 1) == Some(Keyword::SCHEMA)
}

pub(crate) fn parse_alter_type_set_schema(p: &mut Parser) -> Result<Statement> {
    p.expect_keyword_is(Keyword::ALTER).map_err(pe)?;
    p.expect_keyword_is(Keyword::TYPE).map_err(pe)?;
    let name = convert_object_name(p.parse_object_name(false).map_err(pe)?);
    p.expect_keyword_is(Keyword::SET).map_err(pe)?;
    p.expect_keyword_is(Keyword::SCHEMA).map_err(pe)?;
    let schema = convert_ident(p.parse_identifier().map_err(pe)?);
    expect_end(p)?;
    Ok(Statement::Ddl(DdlStatement::AlterType(AlterType {
        name,
        operation: AlterTypeOp::SetSchema(schema),
    })))
}

// ── Utilitaires ───────────────────────────────────────────────────────────────

fn word_or_string(p: &mut Parser) -> Result<String> {
    let tok = p.next_token();
    match tok.token {
        Token::Word(w) => Ok(w.value),
        Token::SingleQuotedString(s) => Ok(s),
        Token::Number(n, _) => Ok(n),
        _ => Err(pe(p.expected_ref::<()>("version", &tok).unwrap_err())),
    }
}

fn signed_integer(p: &mut Parser) -> Result<i64> {
    let negative = p.consume_token(&Token::Minus);
    let tok = p.next_token();
    let Token::Number(n, _) = &tok.token else {
        return Err(pe(p.expected_ref::<()>("entier", &tok).unwrap_err()));
    };
    let value: i64 = n
        .parse()
        .map_err(|_| ParseError::Syntax(format!("entier invalide : {n}")))?;
    Ok(if negative { -value } else { value })
}

fn expect_end(p: &Parser) -> Result<()> {
    if p.peek_token_ref().token == Token::EOF {
        Ok(())
    } else {
        Err(expected(p, "fin du statement"))
    }
}

fn expected(p: &Parser, what: &str) -> ParseError {
    let found = p.peek_token_ref().clone();
    pe(p.expected_ref::<()>(what, &found).unwrap_err())
}

fn pe(e: ParserError) -> ParseError {
    ParseError::Syntax(e.to_string())
}
