//! Grammaire PL/pgSQL (PostgreSQL 18), sur les tokens du dialecte pgweave.
//!
//! Le `Parser` de sqlparser est piloté sur tout le corps : il lit les
//! expressions et les types ; les statements SQL sont délimités sur les
//! tokens puis confiés au pipeline commun (`statement_from_tokens`). Les
//! seules interventions sur les tokens — retirer une clause `INTO`, lire
//! `PERFORM` comme `SELECT` — conservent les positions de tous les autres.

use pgweave_ast::expr::{Expr, Ident, ObjectName};
use pgweave_ast::node::Span;
use pgweave_ast::plpgsql::{
    Block, Declaration, DiagnosticsAssignment, DiagnosticsItem, ExceptionBlock, ExceptionCondition,
    ExceptionHandler, IntoTarget, PlStatement, PlStatementKind, PlpgsqlBody, RaiseLevel,
    RaiseOption, RaiseOptionKey, VarDecl, VarType,
};
use pgweave_ast::stmt::{SelectStmt, Statement};
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Token, TokenWithSpan, Word};

use super::error::PlParseError;
use crate::Offsets;
use crate::convert::{convert_data_type, convert_expr, convert_ident};
use crate::grammar::Ctx;
use crate::pipeline::{DIALECT, statement_from_tokens};
use crate::split::LineIndex;

// ── Points d'entrée ───────────────────────────────────────────────────────────

pub fn parse_plpgsql_body(body: &str) -> Result<PlpgsqlBody, PlParseError> {
    parse_plpgsql_body_mapped(body, 0, &Offsets::Shift(0))
}

/// `file_body_offset` : position du corps dans le document.
pub fn parse_plpgsql_body_at(
    body: &str,
    file_body_offset: usize,
) -> Result<PlpgsqlBody, PlParseError> {
    parse_plpgsql_body_mapped(body, file_body_offset, &Offsets::Shift(0))
}

/// `base` : position du corps dans le texte soumis au parse englobant ;
/// `offsets` : traduction de ce texte vers le document.
pub fn parse_plpgsql_body_mapped(
    body: &str,
    base: usize,
    offsets: &Offsets,
) -> Result<PlpgsqlBody, PlParseError> {
    let _ids = crate::ids::session();
    let _positions = crate::convert::enter_span_ctx(body, base, offsets.clone());
    let index = LineIndex::new(body);
    let tokens = super::tokens::tokenize(body).map_err(|(loc, message)| PlParseError::Syntax {
        pos: index.offset(body, loc).unwrap_or(0),
        message,
    })?;
    let mut pl = Pl {
        parser: Parser::new(&DIALECT).with_tokens_with_locations(tokens.clone()),
        tokens,
        ctx: Ctx {
            text: body,
            index: &index,
        },
        base,
        offsets,
    };
    let label = pl.label()?;
    let block = pl.block(label)?;
    pl.eat_token(&Token::SemiColon);
    if !pl.at_eof() {
        return Err(pl.error("fin du corps après END"));
    }
    Ok(PlpgsqlBody {
        block,
        source: body.to_string(),
    })
}

// ── État ──────────────────────────────────────────────────────────────────────

struct Pl<'a> {
    parser: Parser<'a>,
    /// Copie des tokens du parser, pour délimiter les statements SQL.
    tokens: Vec<TokenWithSpan>,
    ctx: Ctx<'a>,
    base: usize,
    offsets: &'a Offsets,
}

type R<T> = Result<T, PlParseError>;

impl<'a> Pl<'a> {
    // ── Accès aux tokens ─────────────────────────────────────────────────────

    fn peek(&self, n: usize) -> &Token {
        &self.parser.peek_nth_token_ref(n).token
    }

    fn at_eof(&self) -> bool {
        *self.peek(0) == Token::EOF
    }

    /// Mot non quoté en position `n`, en majuscules. Les mots-clés PL/pgSQL
    /// (`LOOP`, `ELSIF`…) ne sont pas tous des mots-clés de sqlparser : on
    /// compare les valeurs.
    fn word(&self, n: usize) -> Option<String> {
        match self.peek(n) {
            Token::Word(Word {
                value,
                quote_style: None,
                ..
            }) => Some(value.to_ascii_uppercase()),
            _ => None,
        }
    }

    fn is_word(&self, n: usize, w: &str) -> bool {
        self.word(n).as_deref() == Some(w)
    }

    fn eat_word(&mut self, w: &str) -> bool {
        if self.is_word(0, w) {
            self.parser.next_token();
            true
        } else {
            false
        }
    }

    fn expect_word(&mut self, w: &str) -> R<()> {
        if self.eat_word(w) {
            Ok(())
        } else {
            Err(self.error(w))
        }
    }

    fn eat_token(&mut self, t: &Token) -> bool {
        self.parser.consume_token(t)
    }

    fn expect_token(&mut self, t: &Token) -> R<()> {
        self.parser
            .expect_token(t)
            .map(|_| ())
            .map_err(|e| self.sql_error(e))
    }

    fn identifier(&mut self) -> R<Ident> {
        self.parser
            .parse_identifier()
            .map(convert_ident)
            .map_err(|e| self.sql_error(e))
    }

    fn expr(&mut self) -> R<Expr> {
        let e = self.parser.parse_expr().map_err(|e| self.sql_error(e))?;
        convert_expr(e).map_err(|e| PlParseError::Unsupported(e.to_string()))
    }

    fn expr_list(&mut self) -> R<Vec<Expr>> {
        let mut out = vec![self.expr()?];
        while self.eat_token(&Token::Comma) {
            out.push(self.expr()?);
        }
        Ok(out)
    }

    /// Chemin `a`, `a.b`, `a.b.c`.
    fn path(&mut self) -> R<Vec<Ident>> {
        let mut out = vec![self.identifier()?];
        while *self.peek(0) == Token::Period {
            self.parser.next_token();
            out.push(self.identifier()?);
        }
        Ok(out)
    }

    // ── Positions ────────────────────────────────────────────────────────────

    fn offset_of(&self, tok: &TokenWithSpan) -> (usize, usize) {
        (
            self.ctx.offset(tok.span.start),
            self.ctx.offset(tok.span.end),
        )
    }

    /// Indice, dans `tokens`, du prochain token significatif à partir de `i`.
    fn next_significant(&self, mut i: usize) -> usize {
        while i < self.tokens.len() && matches!(self.tokens[i].token, Token::Whitespace(_)) {
            i += 1;
        }
        i
    }

    fn span(&self, start_tok: usize, end_tok_exclusive: usize) -> Span {
        let start = self
            .tokens
            .get(start_tok)
            .map(|t| self.offset_of(t).0)
            .unwrap_or(0);
        let end = self.tokens[..end_tok_exclusive.min(self.tokens.len())]
            .iter()
            .rev()
            .find(|t| !matches!(t.token, Token::Whitespace(_)))
            .map(|t| self.offset_of(t).1)
            .unwrap_or(start);
        let (s, e) = self.offsets.span(self.base + start, self.base + end);
        Span {
            start: s as u32,
            end: e as u32,
        }
    }

    // ── Erreurs ──────────────────────────────────────────────────────────────

    fn error(&self, expected: &str) -> PlParseError {
        let tok = self.parser.peek_token_ref();
        PlParseError::Syntax {
            pos: self.offset_of(tok).0,
            message: format!("attendu {expected}, trouvé {}", tok.token),
        }
    }

    fn sql_error(&self, e: ParserError) -> PlParseError {
        PlParseError::Syntax {
            pos: self.offset_of(self.parser.peek_token_ref()).0,
            message: e.to_string(),
        }
    }

    // ── Blocs ────────────────────────────────────────────────────────────────

    /// `<<label>>` éventuel.
    fn label(&mut self) -> R<Option<String>> {
        if *self.peek(0) != Token::ShiftLeft {
            return Ok(None);
        }
        self.parser.next_token();
        let name = self.identifier()?.value;
        self.expect_token(&Token::ShiftRight)?;
        Ok(Some(name))
    }

    /// `[DECLARE …] BEGIN … [EXCEPTION …] END [label]` (le label d'ouverture
    /// est déjà lu).
    fn block(&mut self, label: Option<String>) -> R<Block> {
        let mut declarations = Vec::new();
        if self.eat_word("DECLARE") {
            while !self.is_word(0, "BEGIN") {
                if self.at_eof() {
                    return Err(self.error("BEGIN"));
                }
                // Un second DECLARE est toléré par PostgreSQL.
                if self.eat_word("DECLARE") {
                    continue;
                }
                declarations.push(self.declaration()?);
            }
        }
        self.expect_word("BEGIN")?;
        let statements = self.statements(&["END", "EXCEPTION"])?;
        let exception = if self.eat_word("EXCEPTION") {
            Some(self.exception_block()?)
        } else {
            None
        };
        self.expect_word("END")?;
        // `END label` : facultatif.
        if self.word(0).is_some() && !self.is_word(0, "IF") && !self.is_word(0, "LOOP") {
            self.parser.next_token();
        }
        Ok(Block {
            label,
            declarations,
            statements,
            exception,
        })
    }

    /// Statements jusqu'à l'un des mots de fin (non consommé).
    fn statements(&mut self, until: &[&str]) -> R<Vec<PlStatement>> {
        let mut out = Vec::new();
        loop {
            if self.at_eof() {
                return Err(self.error(&until.join(" ou ")));
            }
            if until.iter().any(|w| self.is_word(0, w)) {
                return Ok(out);
            }
            out.push(self.statement()?);
        }
    }

    fn exception_block(&mut self) -> R<ExceptionBlock> {
        let mut handlers = Vec::new();
        while self.eat_word("WHEN") {
            let mut conditions = vec![self.exception_condition()?];
            while self.eat_word("OR") {
                conditions.push(self.exception_condition()?);
            }
            self.expect_word("THEN")?;
            let statements = self.statements(&["WHEN", "END"])?;
            handlers.push(ExceptionHandler {
                conditions,
                statements,
            });
        }
        Ok(ExceptionBlock { handlers })
    }

    fn exception_condition(&mut self) -> R<ExceptionCondition> {
        if self.eat_word("OTHERS") {
            return Ok(ExceptionCondition::Others);
        }
        if self.eat_word("SQLSTATE") {
            return match self.parser.next_token().token {
                Token::SingleQuotedString(s) => Ok(ExceptionCondition::SqlState(s)),
                _ => Err(self.error("code SQLSTATE")),
            };
        }
        Ok(ExceptionCondition::Named(
            self.identifier()?.value.to_lowercase(),
        ))
    }

    // ── Déclarations ─────────────────────────────────────────────────────────

    fn declaration(&mut self) -> R<Declaration> {
        let start = self.next_significant(self.parser.index());
        let name = self.identifier()?;

        if self.eat_word("ALIAS") {
            self.expect_word("FOR")?;
            let target = self.parser.next_token().token.to_string();
            self.expect_token(&Token::SemiColon)?;
            return Ok(Declaration::Alias { name, target });
        }

        // [NO] SCROLL CURSOR
        let cursor_at = if self.is_word(0, "CURSOR") {
            Some(0)
        } else if self.is_word(0, "SCROLL") && self.is_word(1, "CURSOR") {
            Some(1)
        } else if self.is_word(0, "NO") && self.is_word(1, "SCROLL") && self.is_word(2, "CURSOR") {
            Some(2)
        } else {
            None
        };
        if let Some(skip) = cursor_at {
            for _ in 0..=skip {
                self.parser.next_token();
            }
            let mut params = Vec::new();
            if self.eat_token(&Token::LParen) {
                loop {
                    let p = self.identifier()?;
                    let t = convert_data_type(
                        self.parser
                            .parse_data_type()
                            .map_err(|e| self.sql_error(e))?,
                    )
                    .map_err(|e| PlParseError::Unsupported(e.to_string()))?;
                    params.push((p, t));
                    if !self.eat_token(&Token::Comma) {
                        break;
                    }
                }
                self.expect_token(&Token::RParen)?;
            }
            if !self.eat_word("FOR") {
                self.expect_word("IS")?;
            }
            let query = self.query_until_semicolon()?;
            return Ok(Declaration::Cursor {
                name,
                params,
                query: Box::new(query),
            });
        }

        let constant = self.eat_word("CONSTANT");
        let var_type = self.var_type()?;
        if self.eat_word("COLLATE") {
            self.path()?;
        }
        let not_null = if self.is_word(0, "NOT") && self.is_word(1, "NULL") {
            self.parser.next_token();
            self.parser.next_token();
            true
        } else {
            false
        };
        let default = if self.eat_word("DEFAULT")
            || self.eat_token(&Token::Assignment)
            || self.eat_token(&Token::Eq)
        {
            Some(self.expr()?)
        } else {
            None
        };
        let end = self.parser.index();
        self.expect_token(&Token::SemiColon)?;
        Ok(Declaration::Variable(VarDecl {
            name,
            var_type,
            constant,
            not_null,
            default,
            span: self.span(start, end),
        }))
    }

    fn var_type(&mut self) -> R<VarType> {
        if self.eat_word("RECORD") {
            return Ok(VarType::Record);
        }
        // `nom%ROWTYPE`, `table.colonne%TYPE`, `variable%TYPE` : un chemin
        // suivi de `%`.
        let mut n = 1;
        while *self.peek(n) == Token::Period {
            n += 2;
        }
        if *self.peek(n) == Token::Mod {
            let name = ObjectName(self.path()?);
            self.expect_token(&Token::Mod)?;
            return if self.eat_word("ROWTYPE") {
                Ok(VarType::RowTypeOf(name))
            } else if self.eat_word("TYPE") {
                Ok(VarType::TypeOf(name))
            } else {
                Err(self.error("ROWTYPE ou TYPE"))
            };
        }
        let dt = self
            .parser
            .parse_data_type()
            .map_err(|e| self.sql_error(e))?;
        convert_data_type(dt)
            .map(VarType::Data)
            .map_err(|e| PlParseError::Unsupported(e.to_string()))
    }

    // ── Statements ───────────────────────────────────────────────────────────

    fn statement(&mut self) -> R<PlStatement> {
        let start = self.next_significant(self.parser.index());
        let label = self.label()?;
        let kind = match self.word(0).as_deref() {
            Some("DECLARE") | Some("BEGIN") => {
                let block = self.block(label)?;
                self.expect_token(&Token::SemiColon)?;
                PlStatementKind::Block(block)
            }
            Some("LOOP") | Some("WHILE") | Some("FOR") | Some("FOREACH") => {
                self.loop_statement(label)?
            }
            _ if label.is_some() => {
                return Err(self.error("BEGIN, DECLARE ou une boucle après un label"));
            }
            Some("IF") => self.if_statement()?,
            Some("CASE") => self.case_statement()?,
            Some("RETURN") => self.return_statement()?,
            Some("RAISE") => self.raise_statement()?,
            Some("ASSERT") => {
                self.parser.next_token();
                let condition = self.expr()?;
                let message = if self.eat_token(&Token::Comma) {
                    Some(self.expr()?)
                } else {
                    None
                };
                self.expect_token(&Token::SemiColon)?;
                PlStatementKind::Assert { condition, message }
            }
            Some("EXIT") | Some("CONTINUE") => {
                let exit = self.eat_word("EXIT");
                if !exit {
                    self.parser.next_token();
                }
                let label = if self.word(0).is_some() && !self.is_word(0, "WHEN") {
                    Some(self.identifier()?.value)
                } else {
                    None
                };
                let when = if self.eat_word("WHEN") {
                    Some(self.expr()?)
                } else {
                    None
                };
                self.expect_token(&Token::SemiColon)?;
                if exit {
                    PlStatementKind::Exit { label, when }
                } else {
                    PlStatementKind::Continue { label, when }
                }
            }
            Some("NULL") if *self.peek(1) == Token::SemiColon => {
                self.parser.next_token();
                self.parser.next_token();
                PlStatementKind::Null
            }
            Some("PERFORM") => self.perform()?,
            Some("EXECUTE") => {
                self.parser.next_token();
                let sql = self.expr()?;
                let (into, using) = self.parse_into_and_using()?;
                self.expect_token(&Token::SemiColon)?;
                PlStatementKind::Execute { sql, into, using }
            }
            Some("GET") => self.get_diagnostics()?,
            Some("OPEN") => self.open()?,
            Some("FETCH") => self.fetch()?,
            Some("CLOSE") => {
                self.parser.next_token();
                let cursor = self.identifier()?;
                self.expect_token(&Token::SemiColon)?;
                PlStatementKind::Close(cursor)
            }
            _ if self.is_assignment() => {
                let target = self.path()?;
                if !self.eat_token(&Token::Assignment) {
                    self.expect_token(&Token::Eq)?;
                }
                let value = self.expr()?;
                self.expect_token(&Token::SemiColon)?;
                PlStatementKind::Assign { target, value }
            }
            _ => self.sql_statement()?,
        };
        let end = self.parser.index();
        Ok(PlStatement {
            span: self.span(start, end),
            kind,
        })
    }

    /// `chemin := …` ou `chemin = …` en tête de statement.
    fn is_assignment(&self) -> bool {
        let mut n = 0;
        if !matches!(self.peek(n), Token::Word(_)) {
            return false;
        }
        n += 1;
        while *self.peek(n) == Token::Period && matches!(self.peek(n + 1), Token::Word(_)) {
            n += 2;
        }
        matches!(self.peek(n), Token::Assignment | Token::Eq)
    }

    fn if_statement(&mut self) -> R<PlStatementKind> {
        self.expect_word("IF")?;
        let condition = self.expr()?;
        self.expect_word("THEN")?;
        let then_branch = self.statements(&["ELSIF", "ELSEIF", "ELSE", "END"])?;
        let mut elsif_branches = Vec::new();
        while self.eat_word("ELSIF") || self.eat_word("ELSEIF") {
            let c = self.expr()?;
            self.expect_word("THEN")?;
            elsif_branches.push((c, self.statements(&["ELSIF", "ELSEIF", "ELSE", "END"])?));
        }
        let else_branch = if self.eat_word("ELSE") {
            Some(self.statements(&["END"])?)
        } else {
            None
        };
        self.expect_word("END")?;
        self.expect_word("IF")?;
        self.expect_token(&Token::SemiColon)?;
        Ok(PlStatementKind::If {
            condition,
            then_branch,
            elsif_branches,
            else_branch,
        })
    }

    fn case_statement(&mut self) -> R<PlStatementKind> {
        self.expect_word("CASE")?;
        let operand = if self.is_word(0, "WHEN") {
            None
        } else {
            Some(self.expr()?)
        };
        let mut whens = Vec::new();
        while self.eat_word("WHEN") {
            let values = self.expr_list()?;
            self.expect_word("THEN")?;
            whens.push((values, self.statements(&["WHEN", "ELSE", "END"])?));
        }
        let else_branch = if self.eat_word("ELSE") {
            Some(self.statements(&["END"])?)
        } else {
            None
        };
        self.expect_word("END")?;
        self.expect_word("CASE")?;
        self.expect_token(&Token::SemiColon)?;
        Ok(PlStatementKind::Case {
            operand,
            whens,
            else_branch,
        })
    }

    fn loop_body(&mut self) -> R<Vec<PlStatement>> {
        self.expect_word("LOOP")?;
        let body = self.statements(&["END"])?;
        self.expect_word("END")?;
        self.expect_word("LOOP")?;
        if !matches!(self.peek(0), Token::SemiColon) {
            self.identifier()?; // label de fin
        }
        self.expect_token(&Token::SemiColon)?;
        Ok(body)
    }

    fn loop_statement(&mut self, label: Option<String>) -> R<PlStatementKind> {
        if self.is_word(0, "LOOP") {
            return Ok(PlStatementKind::Loop {
                label,
                body: self.loop_body()?,
            });
        }
        if self.eat_word("WHILE") {
            let condition = self.expr()?;
            return Ok(PlStatementKind::While {
                label,
                condition,
                body: self.loop_body()?,
            });
        }
        if self.eat_word("FOREACH") {
            let targets = self.ident_list()?;
            let slice = if self.eat_word("SLICE") {
                Some(
                    self.parser
                        .parse_literal_uint()
                        .map_err(|e| self.sql_error(e))? as u32,
                )
            } else {
                None
            };
            self.expect_word("IN")?;
            self.expect_word("ARRAY")?;
            let array = self.expr()?;
            return Ok(PlStatementKind::Foreach {
                label,
                targets,
                slice,
                array,
                body: self.loop_body()?,
            });
        }
        self.expect_word("FOR")?;
        let targets = self.ident_list()?;
        self.expect_word("IN")?;

        if self.eat_word("EXECUTE") {
            let sql = self.expr()?;
            let using = if self.eat_word("USING") {
                self.expr_list()?
            } else {
                Vec::new()
            };
            return Ok(PlStatementKind::ForExecute {
                label,
                targets,
                sql,
                using,
                body: self.loop_body()?,
            });
        }

        let start = self.parser.index();
        let loop_at = self
            .find_word_at_depth0(start, "LOOP")
            .ok_or_else(|| self.error("LOOP"))?;
        let slice: Vec<TokenWithSpan> = self.tokens[start..loop_at].to_vec();

        // Intervalle : `..` au premier niveau.
        if let Some(dots) = find_dot_dot(&slice) {
            let [var] = targets.as_slice() else {
                return Err(self.error("une seule variable de boucle"));
            };
            let mut from_tokens = slice[..dots].to_vec();
            let reverse = strip_leading_word(&mut from_tokens, "REVERSE");
            let rest = &slice[dots + 2..];
            let by_at = rest.iter().position(|t| word_is(&t.token, "BY"));
            let (to_tokens, by_tokens) = match by_at {
                Some(i) => (&rest[..i], Some(&rest[i + 1..])),
                None => (rest, None),
            };
            let from = self.expr_from_tokens(from_tokens)?;
            let to = self.expr_from_tokens(to_tokens.to_vec())?;
            let by = by_tokens
                .map(|t| self.expr_from_tokens(t.to_vec()))
                .transpose()?;
            self.advance_to(loop_at);
            return Ok(PlStatementKind::ForRange {
                label,
                var: var.clone(),
                reverse,
                from,
                to,
                by,
                body: self.loop_body()?,
            });
        }

        // Requête : SELECT, WITH, VALUES ou parenthèse.
        let first = slice
            .iter()
            .find(|t| !matches!(t.token, Token::Whitespace(_)));
        let is_query = matches!(first.map(|t| &t.token), Some(Token::LParen))
            || first.is_some_and(|t| {
                word_is(&t.token, "SELECT")
                    || word_is(&t.token, "WITH")
                    || word_is(&t.token, "VALUES")
            });
        if is_query {
            let query = self.select_from_tokens(slice)?;
            self.advance_to(loop_at);
            return Ok(PlStatementKind::ForQuery {
                label,
                targets,
                query,
                body: self.loop_body()?,
            });
        }

        // Curseur lié : `FOR rec IN curseur [(arguments)] LOOP`.
        let [var] = targets.as_slice() else {
            return Err(self.error("une seule variable de boucle sur un curseur"));
        };
        let cursor = self.identifier()?;
        let args = if self.eat_token(&Token::LParen) {
            let a = self.expr_list()?;
            self.expect_token(&Token::RParen)?;
            a
        } else {
            Vec::new()
        };
        Ok(PlStatementKind::ForCursor {
            label,
            var: var.clone(),
            cursor,
            args,
            body: self.loop_body()?,
        })
    }

    fn ident_list(&mut self) -> R<Vec<Ident>> {
        let mut out = vec![self.identifier()?];
        while self.eat_token(&Token::Comma) {
            out.push(self.identifier()?);
        }
        Ok(out)
    }

    fn return_statement(&mut self) -> R<PlStatementKind> {
        self.expect_word("RETURN")?;
        if self.eat_word("NEXT") {
            let e = if *self.peek(0) == Token::SemiColon {
                None
            } else {
                Some(self.expr()?)
            };
            self.expect_token(&Token::SemiColon)?;
            return Ok(PlStatementKind::ReturnNext(e));
        }
        if self.eat_word("QUERY") {
            if self.eat_word("EXECUTE") {
                let sql = self.expr()?;
                let using = if self.eat_word("USING") {
                    self.expr_list()?
                } else {
                    Vec::new()
                };
                self.expect_token(&Token::SemiColon)?;
                return Ok(PlStatementKind::ReturnQueryExecute { sql, using });
            }
            return Ok(PlStatementKind::ReturnQuery(self.query_until_semicolon()?));
        }
        let e = if *self.peek(0) == Token::SemiColon {
            None
        } else {
            Some(self.expr()?)
        };
        self.expect_token(&Token::SemiColon)?;
        Ok(PlStatementKind::Return(e))
    }

    fn raise_statement(&mut self) -> R<PlStatementKind> {
        self.expect_word("RAISE")?;
        let level = match self.word(0).as_deref() {
            Some("DEBUG") => Some(RaiseLevel::Debug),
            Some("LOG") => Some(RaiseLevel::Log),
            Some("INFO") => Some(RaiseLevel::Info),
            Some("NOTICE") => Some(RaiseLevel::Notice),
            Some("WARNING") => Some(RaiseLevel::Warning),
            Some("EXCEPTION") => Some(RaiseLevel::Exception),
            _ => None,
        };
        if level.is_some() {
            self.parser.next_token();
        }
        let level = level.unwrap_or(RaiseLevel::Exception);

        let mut message = None;
        let mut args = Vec::new();
        match self.peek(0).clone() {
            Token::SingleQuotedString(s) | Token::EscapedStringLiteral(s) => {
                self.parser.next_token();
                message = Some(s);
                while self.eat_token(&Token::Comma) {
                    args.push(self.expr()?);
                }
            }
            Token::Word(_) if self.is_word(0, "SQLSTATE") => {
                self.parser.next_token();
                if let Token::SingleQuotedString(s) = self.parser.next_token().token {
                    message = Some(s);
                }
            }
            // Nom de condition (`RAISE unique_violation`).
            Token::Word(_) if !self.is_word(0, "USING") => {
                message = Some(self.identifier()?.value);
            }
            _ => {}
        }

        let mut options = Vec::new();
        if self.eat_word("USING") {
            loop {
                let key = match self.identifier()?.value.to_ascii_uppercase().as_str() {
                    "MESSAGE" => RaiseOptionKey::Message,
                    "DETAIL" => RaiseOptionKey::Detail,
                    "HINT" => RaiseOptionKey::Hint,
                    "ERRCODE" => RaiseOptionKey::Errcode,
                    "COLUMN" => RaiseOptionKey::Column,
                    "CONSTRAINT" => RaiseOptionKey::Constraint,
                    "DATATYPE" => RaiseOptionKey::Datatype,
                    "TABLE" => RaiseOptionKey::Table,
                    "SCHEMA" => RaiseOptionKey::Schema,
                    _ => return Err(self.error("option de RAISE")),
                };
                if !self.eat_token(&Token::Eq) {
                    self.expect_token(&Token::Assignment)?;
                }
                options.push(RaiseOption {
                    key,
                    value: self.expr()?,
                });
                if !self.eat_token(&Token::Comma) {
                    break;
                }
            }
        }
        self.expect_token(&Token::SemiColon)?;
        Ok(PlStatementKind::Raise {
            level,
            message,
            args,
            options,
        })
    }

    fn get_diagnostics(&mut self) -> R<PlStatementKind> {
        self.expect_word("GET")?;
        let stacked = self.eat_word("STACKED");
        let _current = self.eat_word("CURRENT");
        self.expect_word("DIAGNOSTICS")?;
        let mut items = Vec::new();
        loop {
            let var = self.identifier()?;
            if !self.eat_token(&Token::Eq) {
                self.expect_token(&Token::Assignment)?;
            }
            let item = match self.identifier()?.value.to_ascii_uppercase().as_str() {
                "ROW_COUNT" => DiagnosticsItem::RowCount,
                "PG_CONTEXT" => DiagnosticsItem::PgContext,
                "PG_ROUTINE_OID" => DiagnosticsItem::PgRoutineOid,
                "PG_EXCEPTION_CONTEXT" => DiagnosticsItem::PgExceptionContext,
                "PG_EXCEPTION_DETAIL" => DiagnosticsItem::PgExceptionDetail,
                "PG_EXCEPTION_HINT" => DiagnosticsItem::PgExceptionHint,
                "RETURNED_SQLSTATE" => DiagnosticsItem::ReturnedSqlstate,
                "COLUMN_NAME" => DiagnosticsItem::ColumnName,
                "CONSTRAINT_NAME" => DiagnosticsItem::ConstraintName,
                "PG_DATATYPE_NAME" => DiagnosticsItem::PgDatatypeName,
                "MESSAGE_TEXT" => DiagnosticsItem::MessageText,
                "TABLE_NAME" => DiagnosticsItem::TableName,
                "SCHEMA_NAME" => DiagnosticsItem::SchemaName,
                _ => return Err(self.error("élément de GET DIAGNOSTICS")),
            };
            items.push(DiagnosticsAssignment { var, item });
            if !self.eat_token(&Token::Comma) {
                break;
            }
        }
        self.expect_token(&Token::SemiColon)?;
        Ok(PlStatementKind::GetDiagnostics { stacked, items })
    }

    fn open(&mut self) -> R<PlStatementKind> {
        self.expect_word("OPEN")?;
        let cursor = self.identifier()?;
        if self.eat_word("NO") {
            self.expect_word("SCROLL")?;
        } else {
            self.eat_word("SCROLL");
        }
        if self.eat_word("FOR") {
            if self.is_word(0, "EXECUTE") {
                return Err(PlParseError::Unsupported("OPEN … FOR EXECUTE".into()));
            }
            let query = self.query_until_semicolon()?;
            return Ok(PlStatementKind::Open {
                cursor,
                args: Vec::new(),
                query: Some(query),
            });
        }
        let args = if self.eat_token(&Token::LParen) {
            let a = self.expr_list()?;
            self.expect_token(&Token::RParen)?;
            a
        } else {
            Vec::new()
        };
        self.expect_token(&Token::SemiColon)?;
        Ok(PlStatementKind::Open {
            cursor,
            args,
            query: None,
        })
    }

    fn fetch(&mut self) -> R<PlStatementKind> {
        self.expect_word("FETCH")?;
        // Direction : NEXT, PRIOR, FIRST, LAST, ABSOLUTE n, RELATIVE n,
        // FORWARD, BACKWARD, puis FROM / IN.
        loop {
            match self.word(0).as_deref() {
                Some("NEXT" | "PRIOR" | "FIRST" | "LAST" | "FORWARD" | "BACKWARD") => {
                    self.parser.next_token();
                }
                Some("ABSOLUTE" | "RELATIVE") => {
                    self.parser.next_token();
                    self.expr()?;
                }
                Some("FROM" | "IN") => {
                    self.parser.next_token();
                    break;
                }
                _ => break,
            }
        }
        let cursor = self.identifier()?;
        self.expect_word("INTO")?;
        let into = self.parse_into_target()?;
        self.expect_token(&Token::SemiColon)?;
        Ok(PlStatementKind::Fetch { cursor, into })
    }

    /// `[INTO [STRICT] cibles] [USING exprs]`, dans un ordre quelconque.
    fn parse_into_and_using(&mut self) -> R<(Option<IntoTarget>, Vec<Expr>)> {
        let mut into = None;
        let mut using = Vec::new();
        loop {
            if self.eat_word("INTO") {
                into = Some(self.parse_into_target()?);
            } else if self.eat_word("USING") {
                using = self.expr_list()?;
            } else {
                return Ok((into, using));
            }
        }
    }

    fn parse_into_target(&mut self) -> R<IntoTarget> {
        let strict = self.eat_word("STRICT");
        let mut targets = vec![self.path()?];
        while self.eat_token(&Token::Comma) {
            targets.push(self.path()?);
        }
        Ok(IntoTarget { strict, targets })
    }

    // ── SQL ──────────────────────────────────────────────────────────────────

    /// `PERFORM requête` : lu comme `SELECT requête`, sur les mêmes positions.
    fn perform(&mut self) -> R<PlStatementKind> {
        let start = self.next_significant(self.parser.index());
        let end = self.statement_end(start)?;
        let mut tokens = self.tokens[start..end].to_vec();
        if let Token::Word(w) = &mut tokens[0].token {
            *w = Word {
                value: "SELECT".into(),
                quote_style: None,
                keyword: Keyword::SELECT,
            };
        }
        let query = self.select_from_tokens(tokens)?;
        self.advance_to(end);
        self.expect_token(&Token::SemiColon)?;
        Ok(PlStatementKind::Perform(query))
    }

    /// Statement SQL jusqu'au `;`, avec extraction de sa clause `INTO`.
    fn sql_statement(&mut self) -> R<PlStatementKind> {
        let start = self.next_significant(self.parser.index());
        let end = self.statement_end(start)?;
        let mut tokens = self.tokens[start..end].to_vec();
        let into = extract_into(&mut tokens).map_err(|message| PlParseError::Syntax {
            pos: self.offset_of(&self.tokens[start]).0,
            message,
        })?;
        let statement =
            statement_from_tokens(tokens, &self.ctx).map_err(|e| PlParseError::Syntax {
                pos: self.offset_of(&self.tokens[start]).0,
                message: e.to_string(),
            })?;
        self.advance_to(end);
        self.expect_token(&Token::SemiColon)?;
        Ok(PlStatementKind::Sql { statement, into })
    }

    fn query_until_semicolon(&mut self) -> R<SelectStmt> {
        let start = self.next_significant(self.parser.index());
        let end = self.statement_end(start)?;
        let query = self.select_from_tokens(self.tokens[start..end].to_vec())?;
        self.advance_to(end);
        self.expect_token(&Token::SemiColon)?;
        Ok(query)
    }

    fn select_from_tokens(&self, tokens: Vec<TokenWithSpan>) -> R<SelectStmt> {
        let pos = tokens.first().map(|t| self.offset_of(t).0).unwrap_or(0);
        match statement_from_tokens(tokens, &self.ctx) {
            Ok(Statement::Select(s)) => Ok(s),
            Ok(_) => Err(PlParseError::Syntax {
                pos,
                message: "une requête SELECT est attendue".into(),
            }),
            Err(e) => Err(PlParseError::Syntax {
                pos,
                message: e.to_string(),
            }),
        }
    }

    fn expr_from_tokens(&self, tokens: Vec<TokenWithSpan>) -> R<Expr> {
        let pos = tokens.first().map(|t| self.offset_of(t).0).unwrap_or(0);
        let mut p = Parser::new(&DIALECT).with_tokens_with_locations(tokens);
        let e = p.parse_expr().map_err(|e| PlParseError::Syntax {
            pos,
            message: e.to_string(),
        })?;
        if p.peek_token_ref().token != Token::EOF {
            return Err(PlParseError::Syntax {
                pos,
                message: format!("expression : {} inattendu", p.peek_token_ref().token),
            });
        }
        convert_expr(e).map_err(|e| PlParseError::Unsupported(e.to_string()))
    }

    /// Indice du `;` qui termine le statement commençant à `start`.
    fn statement_end(&self, start: usize) -> R<usize> {
        let mut depth = 0i32;
        for (i, t) in self.tokens.iter().enumerate().skip(start) {
            match t.token {
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => depth -= 1,
                Token::SemiColon if depth == 0 => return Ok(i),
                _ => {}
            }
        }
        Err(self.error("; en fin de statement"))
    }

    /// Indice du mot `word` au premier niveau de parenthèses, à partir de
    /// `start`, avant le prochain `;`.
    fn find_word_at_depth0(&self, start: usize, word: &str) -> Option<usize> {
        let mut depth = 0i32;
        for (i, t) in self.tokens.iter().enumerate().skip(start) {
            match &t.token {
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => depth -= 1,
                Token::SemiColon if depth == 0 => return None,
                tok if depth == 0 && word_is(tok, word) => return Some(i),
                _ => {}
            }
        }
        None
    }

    /// Avance le parser jusqu'au token d'indice `target` (non consommé).
    fn advance_to(&mut self, target: usize) {
        while self.next_significant(self.parser.index()) < target {
            self.parser.next_token();
        }
    }
}

// ── Utilitaires sur les tokens ────────────────────────────────────────────────

fn word_is(t: &Token, w: &str) -> bool {
    matches!(t, Token::Word(Word { value, quote_style: None, .. }) if value.eq_ignore_ascii_case(w))
}

fn strip_leading_word(tokens: &mut Vec<TokenWithSpan>, w: &str) -> bool {
    match tokens
        .iter()
        .position(|t| !matches!(t.token, Token::Whitespace(_)))
    {
        Some(i) if word_is(&tokens[i].token, w) => {
            tokens.remove(i);
            true
        }
        _ => false,
    }
}

/// Position du premier `..` (deux points adjacents) au premier niveau.
fn find_dot_dot(tokens: &[TokenWithSpan]) -> Option<usize> {
    let mut depth = 0i32;
    for i in 0..tokens.len() {
        match tokens[i].token {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => depth -= 1,
            Token::Period
                if depth == 0
                    && tokens.get(i + 1).map(|t| &t.token) == Some(&Token::Period)
                    && tokens[i].span.end == tokens[i + 1].span.start =>
            {
                return Some(i);
            }
            _ => {}
        }
    }
    None
}

/// Retire la clause `INTO [STRICT] cibles` d'un statement SQL de corps :
/// - `SELECT … INTO cibles FROM …` : premier `INTO` au premier niveau ;
/// - `INSERT/UPDATE/DELETE … RETURNING … INTO cibles` : `INTO` qui suit
///   `RETURNING` (celui de `INSERT INTO` n'en est pas un).
///
/// Les autres tokens gardent leurs positions.
fn extract_into(tokens: &mut Vec<TokenWithSpan>) -> Result<Option<IntoTarget>, String> {
    let significant: Vec<usize> = (0..tokens.len())
        .filter(|&i| !matches!(tokens[i].token, Token::Whitespace(_)))
        .collect();
    // Verbe principal : le premier token, ou celui qui suit la liste de CTE
    // d'un `WITH` (les corps des CTE sont entre parenthèses).
    const VERBS: [&str; 5] = ["SELECT", "INSERT", "UPDATE", "DELETE", "MERGE"];
    let mut depth = 0i32;
    let verb = significant.iter().map(|&i| &tokens[i].token).find(|t| {
        match t {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => depth -= 1,
            _ => {}
        }
        depth == 0 && VERBS.iter().any(|w| word_is(t, w))
    });
    let Some(verb) = verb else {
        return Ok(None);
    };
    if significant
        .first()
        .is_some_and(|&i| !word_is(&tokens[i].token, "WITH") && &tokens[i].token != verb)
    {
        return Ok(None);
    }
    let is_select = word_is(verb, "SELECT");

    let mut depth = 0i32;
    let mut after_returning = false;
    let mut into_at = None;
    for &i in &significant {
        match &tokens[i].token {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => depth -= 1,
            t if depth == 0 && word_is(t, "RETURNING") => after_returning = true,
            t if depth == 0 && word_is(t, "INTO") && (is_select || after_returning) => {
                into_at = Some(i);
                break;
            }
            _ => {}
        }
    }
    let Some(into_at) = into_at else {
        return Ok(None);
    };

    // Cibles : chemins séparés par des virgules.
    let mut k = significant.iter().position(|&i| i == into_at).unwrap() + 1;
    let tok = |k: usize| significant.get(k).map(|&i| &tokens[i].token);
    let strict = tok(k).is_some_and(|t| word_is(t, "STRICT"));
    if strict {
        k += 1;
    }
    let mut targets = Vec::new();
    loop {
        let mut path = Vec::new();
        loop {
            match tok(k) {
                Some(Token::Word(w)) => {
                    path.push(Ident {
                        value: if w.quote_style.is_some() {
                            w.value.clone()
                        } else {
                            w.value.to_lowercase()
                        },
                        quoted: w.quote_style.is_some(),
                    });
                    k += 1;
                }
                _ => return Err("cible d'INTO attendue".into()),
            }
            if tok(k) == Some(&Token::Period) {
                k += 1;
            } else {
                break;
            }
        }
        targets.push(path);
        if tok(k) == Some(&Token::Comma) {
            k += 1;
        } else {
            break;
        }
    }
    let end = significant.get(k).copied().unwrap_or(tokens.len());
    tokens.drain(into_at..end);
    Ok(Some(IntoTarget { strict, targets }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(src: &str) -> PlpgsqlBody {
        parse_plpgsql_body(src).unwrap_or_else(|e| panic!("{e}\n{src}"))
    }

    fn kinds(b: &PlpgsqlBody) -> Vec<&PlStatementKind> {
        b.block.statements.iter().map(|s| &s.kind).collect()
    }

    #[test]
    fn declarations_de_toutes_sortes() {
        let b = body(
            "DECLARE
                 a CONSTANT integer := 1;
                 b text NOT NULL DEFAULT 'x';
                 r record;
                 p pages%ROWTYPE;
                 t pages.title%TYPE;
                 c CURSOR (k int) FOR SELECT id FROM pages WHERE id = k;
                 al ALIAS FOR $1;
             BEGIN NULL; END",
        );
        let d = &b.block.declarations;
        assert_eq!(d.len(), 7);
        assert!(matches!(&d[2], Declaration::Variable(v) if v.var_type == VarType::Record));
        assert!(
            matches!(&d[3], Declaration::Variable(v) if matches!(v.var_type, VarType::RowTypeOf(_)))
        );
        assert!(
            matches!(&d[4], Declaration::Variable(v) if matches!(v.var_type, VarType::TypeOf(_)))
        );
        assert!(matches!(&d[5], Declaration::Cursor { .. }));
        assert!(matches!(&d[6], Declaration::Alias { .. }));
    }

    #[test]
    fn select_into_et_returning_into() {
        let b = body(
            "DECLARE n int; t text; BEGIN
                 SELECT count(*), max(title) INTO STRICT n, t FROM pages WHERE id > 0;
                 INSERT INTO pages (title) VALUES ('x') RETURNING id INTO n;
                 UPDATE pages SET title = 'y' WHERE id = n RETURNING title INTO t;
             END",
        );
        let k = kinds(&b);
        let PlStatementKind::Sql {
            statement: Statement::Select(s),
            into: Some(into),
        } = k[0]
        else {
            panic!("{:?}", k[0])
        };
        assert!(into.strict);
        assert_eq!(into.targets.len(), 2);
        assert_eq!(s.from.len(), 1, "FROM conservé après retrait de INTO");
        assert!(matches!(
            k[1],
            PlStatementKind::Sql {
                statement: Statement::Insert(_),
                into: Some(_)
            }
        ));
        assert!(matches!(
            k[2],
            PlStatementKind::Sql {
                statement: Statement::Update(_),
                into: Some(_)
            }
        ));
    }

    #[test]
    fn ddl_et_cte_dans_un_corps() {
        // Les constructions qui faisaient échouer l'ancien parser (Mesalia).
        let b = body(
            "BEGIN
                 DROP TABLE IF EXISTS tmp;
                 CREATE TEMP TABLE IF NOT EXISTS tmp (a int) ON COMMIT DROP;
                 WITH x AS (SELECT 1 AS a) INSERT INTO tmp SELECT a FROM x;
                 TRUNCATE tmp;
             END",
        );
        assert_eq!(kinds(&b).len(), 4);
    }

    #[test]
    fn controle_de_flux() {
        let b = body(
            "BEGIN
                 IF a > 1 THEN NULL; ELSIF a = 1 THEN RETURN 1; ELSE RETURN 2; END IF;
                 CASE x WHEN 1, 2 THEN NULL; ELSE NULL; END CASE;
                 <<outer>> LOOP EXIT outer WHEN true; END LOOP outer;
                 WHILE n < 10 LOOP n := n + 1; END LOOP;
                 FOR i IN REVERSE 10..1 BY 2 LOOP CONTINUE WHEN i = 3; END LOOP;
                 FOR r IN SELECT id FROM pages LOOP NULL; END LOOP;
                 FOR r IN EXECUTE 'SELECT 1' USING n LOOP NULL; END LOOP;
                 FOREACH v SLICE 1 IN ARRAY arr LOOP NULL; END LOOP;
                 RETURN QUERY SELECT id FROM pages;
                 RETURN NEXT;
             END",
        );
        let k = kinds(&b);
        assert!(matches!(k[2], PlStatementKind::Loop { label: Some(l), .. } if l == "outer"));
        assert!(matches!(
            k[4],
            PlStatementKind::ForRange {
                reverse: true,
                by: Some(_),
                ..
            }
        ));
        assert!(matches!(k[5], PlStatementKind::ForQuery { .. }));
        assert!(matches!(
            k[7],
            PlStatementKind::Foreach { slice: Some(1), .. }
        ));
    }

    #[test]
    fn case_expression_dans_une_affectation() {
        // Échouait avec l'ancien parser : `CASE … WHEN` dans une expression.
        let b = body("BEGIN v := CASE WHEN a > 0 THEN 1 ELSE 0 END; END");
        assert!(matches!(kinds(&b)[0], PlStatementKind::Assign { .. }));
    }

    #[test]
    fn raise_get_diagnostics_exceptions_curseurs() {
        let b = body(
            "DECLARE c CURSOR FOR SELECT 1; r record; n int; BEGIN
                 RAISE NOTICE 'n = %, m = %', n, n + 1 USING HINT = 'h';
                 RAISE unique_violation;
                 RAISE;
                 GET DIAGNOSTICS n = ROW_COUNT;
                 OPEN c; FETCH NEXT FROM c INTO r; CLOSE c;
                 PERFORM pg_sleep(0);
                 ASSERT n > 0, 'positif';
             EXCEPTION
                 WHEN unique_violation OR SQLSTATE '23503' THEN NULL;
                 WHEN OTHERS THEN RAISE;
             END",
        );
        assert_eq!(b.block.exception.as_ref().unwrap().handlers.len(), 2);
        assert_eq!(kinds(&b).len(), 9);
    }

    #[test]
    fn affectation_a_new() {
        let b = body("BEGIN NEW.updated_at = now(); RETURN NEW; END");
        let PlStatementKind::Assign { target, .. } = kinds(&b)[0] else {
            panic!()
        };
        assert_eq!(target.len(), 2);
    }

    #[test]
    fn blocs_imbriques_avec_leurs_declarations() {
        let b = body("BEGIN DECLARE x int := 1; BEGIN RETURN x; END; END");
        let PlStatementKind::Block(inner) = kinds(&b)[0] else {
            panic!()
        };
        assert_eq!(inner.declarations.len(), 1);
    }

    #[test]
    fn les_positions_visent_le_corps() {
        let src = "BEGIN\n  UPDATE pages SET title = nope WHERE id = 1;\nEND";
        let b = parse_plpgsql_body_at(src, 100).unwrap();
        let s = &b.block.statements[0];
        assert_eq!(s.span.start as usize, 100 + src.find("UPDATE").unwrap());
        let PlStatementKind::Sql {
            statement: Statement::Update(u),
            ..
        } = &s.kind
        else {
            panic!()
        };
        let span = u.assignments[0].value.span;
        assert_eq!(
            &src[span.start as usize - 100..span.end as usize - 100],
            "nope"
        );
    }

    #[test]
    fn erreur_signalee() {
        assert!(parse_plpgsql_body("BEGIN IF THEN END IF; END").is_err());
        assert!(parse_plpgsql_body("BEGIN NULL;").is_err());
    }
}
