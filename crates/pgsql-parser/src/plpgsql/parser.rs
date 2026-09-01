use pgsql_ast::{
    expr::Ident,
    plpgsql::{
        AssignTarget, Block, DiagnosticsAssignment, DiagnosticsItem, ElsIf, ExceptionBlock,
        ExceptionCondition, ExceptionHandler, PlExpr, PlStatement, PlpgsqlBody, RaiseLevel,
        RaiseOption, RaiseOptionKey, VarDecl,
    },
    types::DataType,
};

use super::{
    error::PlParseError,
    keywords::Keyword,
    lexer::{Lexer, Token},
};
use pgsql_ast::expr::{Expr, ExprKind};

// ── Public entry point ──────────────────────────────────────────────────────

pub fn parse_plpgsql_body(body: &str) -> Result<PlpgsqlBody, PlParseError> {
    parse_plpgsql_body_at(body, 0)
}

/// Like `parse_plpgsql_body` but takes the byte offset of the body string
/// within the full source file. Used to produce file-accurate diagnostic spans
/// for SQL fragments embedded in PL/pgSQL bodies.
pub fn parse_plpgsql_body_at(
    body: &str,
    file_body_offset: usize,
) -> Result<PlpgsqlBody, PlParseError> {
    parse_plpgsql_body_mapped(body, file_body_offset, &crate::Offsets::Shift(0))
}

/// Comme [`parse_plpgsql_body_at`], mais la position finale de chaque statement
/// interne passe par `offsets`.
///
/// `body_offset_in_input` situe le corps dans le texte soumis au parser, **non
/// traduit** : c'est la position de chaque statement interne qui est traduite,
/// parce qu'elle tombe dans un fragment recopié, là où la position du `$$` est
/// du texte engendré sans antécédent.
pub fn parse_plpgsql_body_mapped(
    body: &str,
    body_offset_in_input: usize,
    offsets: &crate::Offsets,
) -> Result<PlpgsqlBody, PlParseError> {
    let file_body_offset = body_offset_in_input;
    let offsets = offsets.clone();
    let mut lexer = Lexer::new(body);
    let (tokens, token_offsets) = lexer.tokenize_all_with_offsets();
    let mut parser = Parser {
        tokens,
        pos: 0,
        source: body.to_string(),
        token_offsets,
        file_body_offset,
        offsets,
    };
    parser.parse_body()
}

// ── Parser struct ───────────────────────────────────────────────────────────

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    #[allow(dead_code)]
    source: String,
    /// Byte offset of each token within the body string.
    token_offsets: Vec<usize>,
    /// Byte offset of the body string within the full source file.
    /// Used to shift SQL fragment spans to their file-absolute positions.
    file_body_offset: usize,
    /// Traduction des positions vers le repère du document.
    offsets: crate::Offsets,
}

impl Parser {
    // ── Token access helpers ──────────────────────────────────────────────

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    #[allow(dead_code)] // helper de lookahead, conservé pour les règles à venir
    fn peek2(&self) -> &Token {
        self.tokens.get(self.pos + 1).unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) -> Token {
        let t = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        if self.pos < self.tokens.len() {
            self.pos += 1;
        }
        t
    }

    #[allow(dead_code)]
    fn current_keyword(&self) -> Option<Keyword> {
        match self.peek() {
            Token::Keyword(k) => Some(k.clone()),
            _ => None,
        }
    }

    /// Consume the next token if it matches the given keyword. Returns true if consumed.
    fn eat_keyword(&mut self, kw: Keyword) -> bool {
        if self.peek() == &Token::Keyword(kw) {
            self.advance();
            true
        } else {
            false
        }
    }

    /// Consume the next token if it matches the given token. Returns true if consumed.
    fn eat_token(&mut self, t: &Token) -> bool {
        if self.peek() == t {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect_keyword(&mut self, kw: Keyword) -> Result<(), PlParseError> {
        if self.eat_keyword(kw.clone()) {
            Ok(())
        } else {
            Err(PlParseError::UnexpectedToken {
                token: format!("{:?}", self.peek()),
                pos: self.pos,
            })
        }
    }

    #[allow(dead_code)]
    fn expect_semi(&mut self) -> Result<(), PlParseError> {
        if self.eat_token(&Token::Semi) {
            Ok(())
        } else {
            Err(PlParseError::UnexpectedToken {
                token: format!("{:?}", self.peek()),
                pos: self.pos,
            })
        }
    }

    fn expect_token(&mut self, t: Token) -> Result<(), PlParseError> {
        if self.eat_token(&t) {
            Ok(())
        } else {
            Err(PlParseError::UnexpectedToken {
                token: format!("{:?}", self.peek()),
                pos: self.pos,
            })
        }
    }

    fn expect_ident(&mut self) -> Result<String, PlParseError> {
        match self.advance() {
            Token::Ident(s) => Ok(s),
            Token::Keyword(k) => {
                // Many identifiers are keywords in PL/pgSQL but used as names
                Ok(format!("{:?}", k).to_lowercase())
            }
            other => Err(PlParseError::UnexpectedToken {
                token: format!("{:?}", other),
                pos: self.pos,
            }),
        }
    }

    fn expect_string_lit(&mut self) -> Result<String, PlParseError> {
        match self.advance() {
            Token::StringLit(s) => Ok(s),
            other => Err(PlParseError::UnexpectedToken {
                token: format!("{:?}", other),
                pos: self.pos,
            }),
        }
    }

    // ── Body ─────────────────────────────────────────────────────────────

    fn parse_body(&mut self) -> Result<PlpgsqlBody, PlParseError> {
        let declare = if self.peek() == &Token::Keyword(Keyword::Declare) {
            self.advance(); // consume DECLARE
            self.parse_declare_section()?
        } else {
            vec![]
        };

        self.expect_keyword(Keyword::Begin)?;
        let stmts = self.parse_statements()?;
        let exception = self.parse_exception_block()?;
        self.expect_keyword(Keyword::End)?;
        // Optional trailing semicolon
        self.eat_token(&Token::Semi);

        Ok(PlpgsqlBody {
            declare,
            block: Block {
                label: None,
                statements: stmts,
                exception,
            },
        })
    }

    // ── DECLARE section ───────────────────────────────────────────────────

    fn parse_declare_section(&mut self) -> Result<Vec<VarDecl>, PlParseError> {
        let mut decls = Vec::new();
        loop {
            match self.peek() {
                Token::Keyword(Keyword::Begin) | Token::Eof => break,
                _ => decls.push(self.parse_var_decl()?),
            }
        }
        Ok(decls)
    }

    fn parse_var_decl(&mut self) -> Result<VarDecl, PlParseError> {
        // Variable name (can be keyword-like)
        let name = match self.advance() {
            Token::Ident(s) => s,
            Token::Keyword(k) => format!("{:?}", k).to_lowercase(),
            other => {
                return Err(PlParseError::UnexpectedToken {
                    token: format!("{:?}", other),
                    pos: self.pos,
                });
            }
        };

        // Optional CONSTANT
        let constant = match self.peek() {
            Token::Ident(s) if s.to_uppercase() == "CONSTANT" => {
                self.advance();
                true
            }
            _ => false,
        };

        // Collect type tokens until ';', ':=', or 'DEFAULT'
        let data_type = self.parse_var_type()?;

        // Optional NOT NULL
        let not_null = if self.eat_keyword(Keyword::Not) {
            // expect NULL token — in lexer Keyword::Null
            self.eat_keyword(Keyword::Null);
            true
        } else {
            false
        };

        // Optional default
        let default = if self.eat_token(&Token::ColonEq)
            || self.eat_token(&Token::Assign)
            || self.eat_token(&Token::Eq)
            || self.eat_keyword_ident("DEFAULT")
        {
            Some(self.parse_plexpr_until_semi()?)
        } else {
            None
        };

        self.eat_token(&Token::Semi);

        Ok(VarDecl {
            name: Ident::new(name),
            data_type,
            not_null,
            constant,
            default,
        })
    }

    /// Eat an identifier token whose value (case-insensitive) matches s.
    fn eat_keyword_ident(&mut self, s: &str) -> bool {
        match self.peek() {
            Token::Ident(v) if v.to_uppercase() == s.to_uppercase() => {
                self.advance();
                true
            }
            _ => false,
        }
    }

    /// Parse a type declaration. Reads tokens until ';', ':=', 'DEFAULT' (as ident),
    /// or 'NOT' (for NOT NULL), then converts to DataType.
    fn parse_var_type(&mut self) -> Result<DataType, PlParseError> {
        let mut parts: Vec<String> = Vec::new();
        let mut depth = 0i32;

        loop {
            match self.peek() {
                Token::Semi if depth == 0 => break,
                Token::ColonEq | Token::Assign => break,
                Token::Eq if depth == 0 => break,
                Token::Keyword(Keyword::Not) if depth == 0 => break,
                Token::Keyword(Keyword::Null) if depth == 0 && !parts.is_empty() => break,
                Token::Eof => break,
                Token::Ident(s) if s.to_uppercase() == "DEFAULT" && depth == 0 => break,
                Token::LParen => {
                    depth += 1;
                    parts.push(self.token_to_str_consume());
                }
                Token::RParen => {
                    if depth > 0 {
                        depth -= 1;
                        parts.push(self.token_to_str_consume());
                    } else {
                        break;
                    }
                }
                _ => parts.push(self.token_to_str_consume()),
            }
        }

        let type_str = parts.join(" ").trim().to_string();
        Ok(self.str_to_datatype(&type_str))
    }

    fn str_to_datatype(&self, s: &str) -> DataType {
        let upper = s.to_uppercase();
        // Handle %ROWTYPE and %TYPE annotations
        if upper.ends_with("%ROWTYPE") || upper.ends_with("% ROWTYPE") {
            let table = s.split('%').next().unwrap_or("").trim();
            return DataType::Custom(format!("{}%ROWTYPE", table));
        }
        if upper.ends_with("%TYPE") || upper.ends_with("% TYPE") {
            let col = s.split('%').next().unwrap_or("").trim();
            return DataType::Custom(format!("{}%TYPE", col));
        }
        match upper.as_str() {
            "UUID" => DataType::Uuid,
            "TEXT" => DataType::Text,
            "INTEGER" | "INT" | "INT4" => DataType::Integer,
            "BIGINT" | "INT8" => DataType::BigInt,
            "SMALLINT" | "INT2" => DataType::SmallInt,
            "BOOLEAN" | "BOOL" => DataType::Boolean,
            "TIMESTAMPTZ" | "TIMESTAMP WITH TIME ZONE" => DataType::TimestampTz,
            "TIMESTAMP" => DataType::Timestamp,
            "DATE" => DataType::Date,
            "TIME" => DataType::Time,
            "REAL" | "FLOAT4" => DataType::Real,
            "DOUBLE PRECISION" | "FLOAT8" => DataType::DoublePrecision,
            "JSON" => DataType::Json,
            "JSONB" => DataType::Jsonb,
            "BYTEA" => DataType::Bytea,
            "INTERVAL" => DataType::Interval,
            _ => DataType::Custom(s.to_string()),
        }
    }

    // ── Statements ────────────────────────────────────────────────────────

    fn parse_statements(&mut self) -> Result<Vec<PlStatement>, PlParseError> {
        let mut stmts = Vec::new();
        loop {
            match self.peek() {
                Token::Keyword(Keyword::End)
                | Token::Keyword(Keyword::Elsif)
                | Token::Keyword(Keyword::Else)
                | Token::Keyword(Keyword::Exception)
                | Token::Eof => break,
                _ => stmts.push(self.parse_statement()?),
            }
        }
        Ok(stmts)
    }

    fn parse_statement(&mut self) -> Result<PlStatement, PlParseError> {
        match self.peek().clone() {
            Token::Keyword(Keyword::If) => self.parse_if(),
            Token::Keyword(Keyword::Raise) => self.parse_raise(),
            Token::Keyword(Keyword::Return) => self.parse_return(),
            Token::Keyword(Keyword::Loop) => self.parse_loop(),
            Token::Keyword(Keyword::While) => self.parse_while(),
            Token::Keyword(Keyword::For) => self.parse_for(),
            Token::Keyword(Keyword::Exit) => self.parse_exit_continue(true),
            Token::Keyword(Keyword::Continue) => self.parse_exit_continue(false),
            Token::Keyword(Keyword::Null) => {
                self.advance();
                self.eat_token(&Token::Semi);
                Ok(PlStatement::Null)
            }
            Token::Keyword(Keyword::Select) => self.parse_select_into_or_perform(),
            Token::Keyword(Keyword::Insert)
            | Token::Keyword(Keyword::Update)
            | Token::Keyword(Keyword::Delete) => self.parse_dml_statement(),
            Token::Keyword(Keyword::Perform) => {
                self.advance();
                let fragment = self.extract_until_semi_balanced();
                self.eat_token(&Token::Semi);
                let expr = self.expr_fragment_to_plexpr(&fragment)?;
                Ok(PlStatement::Perform(expr))
            }
            Token::Keyword(Keyword::Execute) => self.parse_execute(),
            Token::Keyword(Keyword::Call) => self.parse_call(),
            Token::Keyword(Keyword::Get) => self.parse_get_diagnostics(),
            Token::Keyword(Keyword::Begin) => {
                // Nested block
                self.advance(); // consume BEGIN
                let stmts = self.parse_statements()?;
                let exception = self.parse_exception_block()?;
                self.expect_keyword(Keyword::End)?;
                self.eat_token(&Token::Semi);
                Ok(PlStatement::SubBlock(Block {
                    label: None,
                    statements: stmts,
                    exception,
                }))
            }
            Token::Keyword(Keyword::New) | Token::Keyword(Keyword::Old) | Token::Ident(_) => {
                self.parse_assign_or_sql()
            }
            other => Err(PlParseError::UnexpectedToken {
                token: format!("{:?}", other),
                pos: self.pos,
            }),
        }
    }

    // ── IF ────────────────────────────────────────────────────────────────

    fn parse_if(&mut self) -> Result<PlStatement, PlParseError> {
        self.expect_keyword(Keyword::If)?;
        let condition = self.parse_plexpr_until_keyword(Keyword::Then)?;
        self.expect_keyword(Keyword::Then)?;
        let then_stmts = self.parse_statements()?;
        let then_block = Block {
            label: None,
            statements: then_stmts,
            exception: None,
        };

        let mut elsif_clauses = Vec::new();
        let mut else_block = None;

        loop {
            if self.eat_keyword(Keyword::Elsif) {
                let cond = self.parse_plexpr_until_keyword(Keyword::Then)?;
                self.expect_keyword(Keyword::Then)?;
                let stmts = self.parse_statements()?;
                elsif_clauses.push(ElsIf {
                    condition: cond,
                    block: Block {
                        label: None,
                        statements: stmts,
                        exception: None,
                    },
                });
            } else if self.eat_keyword(Keyword::Else) {
                let stmts = self.parse_statements()?;
                else_block = Some(Block {
                    label: None,
                    statements: stmts,
                    exception: None,
                });
                break;
            } else {
                break;
            }
        }

        self.expect_keyword(Keyword::End)?;
        self.expect_keyword(Keyword::If)?;
        self.eat_token(&Token::Semi);

        Ok(PlStatement::If {
            condition,
            then_block,
            elsif_clauses,
            else_block,
        })
    }

    // ── RAISE ─────────────────────────────────────────────────────────────

    fn parse_raise(&mut self) -> Result<PlStatement, PlParseError> {
        self.expect_keyword(Keyword::Raise)?;

        // Optional level
        let level = match self.peek() {
            Token::Keyword(Keyword::Exception) => {
                self.advance();
                RaiseLevel::Exception
            }
            Token::Keyword(Keyword::Warning) => {
                self.advance();
                RaiseLevel::Warning
            }
            Token::Keyword(Keyword::Notice) => {
                self.advance();
                RaiseLevel::Notice
            }
            Token::Keyword(Keyword::Info) => {
                self.advance();
                RaiseLevel::Info
            }
            Token::Keyword(Keyword::Log) => {
                self.advance();
                RaiseLevel::Log
            }
            Token::Keyword(Keyword::Debug) => {
                self.advance();
                RaiseLevel::Debug
            }
            _ => RaiseLevel::Exception, // default
        };

        // Check for USING options (no message string)
        if self.peek() == &Token::Keyword(Keyword::Using) {
            self.advance();
            let options = self.parse_raise_options()?;
            self.eat_token(&Token::Semi);
            return Ok(PlStatement::Raise {
                level,
                message: None,
                args: vec![],
                options,
            });
        }

        // Message string literal
        let (message, args, options) = if let Token::StringLit(_) = self.peek().clone() {
            let msg = match self.advance() {
                Token::StringLit(s) => s,
                _ => unreachable!(),
            };
            let mut args = Vec::new();
            while self.eat_token(&Token::Comma) {
                // A USING after a comma ends the arg list and begins options.
                if self.peek() == &Token::Keyword(Keyword::Using) {
                    self.advance();
                    let opts = self.parse_raise_options()?;
                    self.eat_token(&Token::Semi);
                    return Ok(PlStatement::Raise {
                        level,
                        message: Some(msg),
                        args,
                        options: opts,
                    });
                }
                let atom = self.parse_plexpr_atom()?;
                args.push(atom);
            }
            // USING can follow the message/args without a preceding comma:
            //   RAISE exception 'msg' USING ERRCODE = '...';
            let opts = if self.peek() == &Token::Keyword(Keyword::Using) {
                self.advance();
                self.parse_raise_options()?
            } else {
                vec![]
            };
            (Some(msg), args, opts)
        } else {
            // Re-raise with no args (just RAISE;)
            (None, vec![], vec![])
        };

        self.eat_token(&Token::Semi);
        Ok(PlStatement::Raise {
            level,
            message,
            args,
            options,
        })
    }

    fn parse_raise_options(&mut self) -> Result<Vec<RaiseOption>, PlParseError> {
        let mut options = Vec::new();
        loop {
            let key = match self.peek() {
                Token::Keyword(Keyword::Message) => {
                    self.advance();
                    RaiseOptionKey::Message
                }
                Token::Keyword(Keyword::Detail) => {
                    self.advance();
                    RaiseOptionKey::Detail
                }
                Token::Keyword(Keyword::Hint) => {
                    self.advance();
                    RaiseOptionKey::Hint
                }
                Token::Keyword(Keyword::Errcode) => {
                    self.advance();
                    RaiseOptionKey::Errcode
                }
                Token::Keyword(Keyword::Column) => {
                    self.advance();
                    RaiseOptionKey::Column
                }
                Token::Keyword(Keyword::Constraint) => {
                    self.advance();
                    RaiseOptionKey::Constraint
                }
                Token::Keyword(Keyword::Datatype) => {
                    self.advance();
                    RaiseOptionKey::Datatype
                }
                Token::Keyword(Keyword::Table) => {
                    self.advance();
                    RaiseOptionKey::Table
                }
                Token::Keyword(Keyword::Schema) => {
                    self.advance();
                    RaiseOptionKey::Schema
                }
                _ => break,
            };
            self.eat_token(&Token::Eq);
            let val = self.parse_plexpr_atom()?;
            options.push(RaiseOption { key, value: val });
            if !self.eat_token(&Token::Comma) {
                break;
            }
        }
        Ok(options)
    }

    // ── RETURN ────────────────────────────────────────────────────────────

    fn parse_return(&mut self) -> Result<PlStatement, PlParseError> {
        self.expect_keyword(Keyword::Return)?;

        // RETURN NEXT
        if self.eat_keyword(Keyword::ReturnNext) {
            let expr = self.parse_plexpr_until_semi()?;
            self.eat_token(&Token::Semi);
            return Ok(PlStatement::ReturnNext(expr));
        }

        // RETURN QUERY
        if self.eat_keyword(Keyword::ReturnQuery) {
            let frag_start = self.pos;
            let fragment = self.extract_until_semi_balanced();
            self.eat_token(&Token::Semi);
            match self.parse_sql_fragment(&fragment, frag_start) {
                Ok(pgsql_ast::Statement::Select(sel)) => {
                    return Ok(PlStatement::ReturnQuery(sel));
                }
                _ => {
                    return Err(PlParseError::Unsupported(format!(
                        "RETURN QUERY: cannot parse SELECT: {}",
                        &fragment[..fragment.len().min(60)]
                    )));
                }
            }
        }

        // RETURN; (empty)
        if self.peek() == &Token::Semi || self.peek() == &Token::Eof {
            self.eat_token(&Token::Semi);
            return Ok(PlStatement::Return(None));
        }

        // RETURN expr;
        let expr = self.parse_plexpr_until_semi()?;
        self.eat_token(&Token::Semi);
        Ok(PlStatement::Return(Some(expr)))
    }

    // ── Assign or SQL ─────────────────────────────────────────────────────

    /// Determine if the upcoming token sequence looks like an assignment.
    /// Peeks ahead (up to a few tokens) to detect := or = after a target.
    fn is_assignment(&self) -> bool {
        let mut i = self.pos;

        // Skip the target part: [NEW|OLD|ident] [. ident]*
        let is_target_start = matches!(
            self.tokens.get(i),
            Some(Token::Keyword(Keyword::New) | Token::Keyword(Keyword::Old) | Token::Ident(_))
        );
        if !is_target_start {
            return false;
        }
        i += 1;

        // Optional .field chains
        while let (Some(Token::Dot), Some(Token::Ident(_) | Token::Keyword(_))) =
            (self.tokens.get(i), self.tokens.get(i + 1))
        {
            i += 2;
        }

        // Optional subscript [...]
        if matches!(self.tokens.get(i), Some(Token::LBracket)) {
            i += 1;
            let mut depth = 1;
            while i < self.tokens.len() && depth > 0 {
                match self.tokens.get(i) {
                    Some(Token::LBracket) => depth += 1,
                    Some(Token::RBracket) => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
        }

        matches!(
            self.tokens.get(i),
            Some(Token::ColonEq) | Some(Token::Assign) | Some(Token::Eq)
        )
    }

    fn parse_assign_or_sql(&mut self) -> Result<PlStatement, PlParseError> {
        if self.is_assignment() {
            // Parse target
            let target = match self.peek().clone() {
                Token::Keyword(Keyword::New) => {
                    self.advance();
                    self.expect_token(Token::Dot)?;
                    let field = self.expect_ident()?;
                    AssignTarget::NewField(Ident::new(field))
                }
                Token::Keyword(Keyword::Old) => {
                    self.advance();
                    self.expect_token(Token::Dot)?;
                    let field = self.expect_ident()?;
                    AssignTarget::OldField(Ident::new(field))
                }
                Token::Ident(name) => {
                    let name = name.clone();
                    self.advance();
                    if self.eat_token(&Token::Dot) {
                        let field = self.expect_ident()?;
                        AssignTarget::Field(Ident::new(name), Ident::new(field))
                    } else {
                        AssignTarget::Simple(Ident::new(name))
                    }
                }
                _ => {
                    return Err(PlParseError::Unsupported(
                        "unexpected token in assignment target".to_string(),
                    ));
                }
            };

            // Consume := or =
            if !self.eat_token(&Token::ColonEq)
                && !self.eat_token(&Token::Assign)
                && !self.eat_token(&Token::Eq)
            {
                return Err(PlParseError::UnexpectedToken {
                    token: format!("{:?}", self.peek()),
                    pos: self.pos,
                });
            }

            let value = self.parse_plexpr_until_semi()?;
            self.eat_token(&Token::Semi);
            Ok(PlStatement::Assign { target, value })
        } else {
            // Not an assignment — treat as a raw SQL fragment
            let frag_start = self.pos;
            let fragment = self.extract_until_semi_balanced();
            self.eat_token(&Token::Semi);
            match self.parse_sql_fragment(&fragment, frag_start) {
                Ok(stmt) => Ok(PlStatement::Sql(stmt)),
                Err(e) => Err(PlParseError::Unsupported(format!(
                    "cannot parse SQL statement: {} (fragment: {})",
                    e,
                    &fragment[..fragment.len().min(60)]
                ))),
            }
        }
    }

    // ── SELECT INTO / PERFORM ─────────────────────────────────────────────

    fn parse_select_into_or_perform(&mut self) -> Result<PlStatement, PlParseError> {
        let frag_start = self.pos;
        let fragment = self.extract_until_semi_balanced();
        self.eat_token(&Token::Semi);

        let upper = fragment.to_uppercase();

        // Detect SELECT ... INTO [STRICT] var FROM ...
        // We look for INTO that is not part of INSERT INTO
        if let Some(into_pos) = find_into_position(&upper) {
            let after_into_trimmed = fragment[into_pos..].trim_start();
            // Skip "INTO" keyword
            let after_kw = after_into_trimmed[4..].trim_start();

            // Check for STRICT
            let (strict, targets_str_start) = if after_kw.to_uppercase().starts_with("STRICT") {
                (true, after_kw[6..].trim_start())
            } else {
                (false, after_kw)
            };

            // The targets are identifiers before FROM, WHERE, or end of string
            let upper_after = targets_str_start.to_uppercase();
            let targets_end = upper_after
                .find(" FROM ")
                .or_else(|| upper_after.find("\nFROM "))
                .or_else(|| upper_after.find(" WHERE "))
                .or_else(|| upper_after.find(" LIMIT "))
                .unwrap_or(targets_str_start.len());

            let targets_str = &targets_str_start[..targets_end];
            let targets: Vec<Ident> = targets_str
                .split(',')
                .map(|s| Ident::new(s.trim()))
                .filter(|id| !id.value.is_empty())
                .collect();

            if !targets.is_empty() {
                // sqlparser ne connaît pas la clause INTO de PL/pgSQL : on la
                // neutralise avant de lui soumettre la requête.
                //
                // On la **blanchit** — remplacée par autant d'espaces — au lieu
                // de la retirer. Retirer le texte décalait tout ce qui suit de
                // la longueur de « INTO n », et comme `parse_sql_fragment`
                // applique un décalage *constant*, les spans du reste de la
                // requête tombaient à côté : un `WHERE col_inexistante` était
                // rapporté à une position arbitraire, souvent sur la première
                // ligne du fichier. Blanchir préserve longueur et offsets, donc
                // le décalage constant redevient exact.
                //
                // `targets_str_start` dérive de `fragment` par des opérations de
                // suffixe uniquement, donc son offset se déduit des longueurs.
                let targets_offset = fragment.len() - targets_str_start.len();
                let into_end = (targets_offset + targets_end).min(fragment.len());

                let mut select_without_into = String::with_capacity(fragment.len());
                select_without_into.push_str(&fragment[..into_pos]);
                select_without_into
                    .extend(std::iter::repeat_n(' ', into_end.saturating_sub(into_pos)));
                select_without_into.push_str(&fragment[into_end..]);
                debug_assert_eq!(
                    select_without_into.len(),
                    fragment.len(),
                    "le blanchiment doit préserver la longueur"
                );

                if let Ok(stmt) = self.parse_sql_fragment(&select_without_into, frag_start)
                    && let pgsql_ast::Statement::Select(sel) = stmt
                {
                    return Ok(PlStatement::SelectInto {
                        strict,
                        targets,
                        query: sel,
                    });
                }
            }
        }

        // Fallback: regular SQL statement
        match self.parse_sql_fragment(&fragment, frag_start) {
            Ok(stmt) => Ok(PlStatement::Sql(stmt)),
            Err(e) => Err(PlParseError::Unsupported(format!(
                "cannot parse SQL fragment: {} | fragment: {}",
                e,
                &fragment[..fragment.len().min(80)]
            ))),
        }
    }

    fn parse_dml_statement(&mut self) -> Result<PlStatement, PlParseError> {
        let frag_start = self.pos;
        let fragment = self.extract_until_semi_balanced();
        self.eat_token(&Token::Semi);
        // Strip `RETURNING ... INTO var` — PL/pgSQL captures RETURNING results
        // into variables with `RETURNING cols INTO var`, but sqlparser only
        // understands standard SQL `RETURNING cols`. Strip the `INTO var` tail.
        let clean = strip_returning_into(&fragment);
        match self.parse_sql_fragment(clean, frag_start) {
            Ok(stmt) => Ok(PlStatement::Sql(stmt)),
            Err(e) => Err(PlParseError::Unsupported(format!(
                "cannot parse DML: {} | fragment: {}",
                e,
                &fragment[..fragment.len().min(80)]
            ))),
        }
    }

    // ── LOOP / WHILE / FOR ────────────────────────────────────────────────

    fn parse_loop(&mut self) -> Result<PlStatement, PlParseError> {
        self.expect_keyword(Keyword::Loop)?;
        let stmts = self.parse_statements()?;
        self.expect_keyword(Keyword::End)?;
        self.expect_keyword(Keyword::Loop)?;
        self.eat_token(&Token::Semi);
        Ok(PlStatement::Loop(Block {
            label: None,
            statements: stmts,
            exception: None,
        }))
    }

    fn parse_while(&mut self) -> Result<PlStatement, PlParseError> {
        self.expect_keyword(Keyword::While)?;
        let cond = self.parse_plexpr_until_keyword(Keyword::Loop)?;
        self.expect_keyword(Keyword::Loop)?;
        let stmts = self.parse_statements()?;
        self.expect_keyword(Keyword::End)?;
        self.expect_keyword(Keyword::Loop)?;
        self.eat_token(&Token::Semi);
        Ok(PlStatement::While {
            condition: cond,
            body: Block {
                label: None,
                statements: stmts,
                exception: None,
            },
        })
    }

    fn parse_for(&mut self) -> Result<PlStatement, PlParseError> {
        self.expect_keyword(Keyword::For)?;
        let var_name = self.expect_ident()?;
        let var = Ident::new(var_name);
        self.expect_keyword(Keyword::In)?;

        // Check for REVERSE
        let reverse = self.eat_keyword(Keyword::Reverse);

        // Check if this is a range FOR (expr .. expr) or query FOR
        // Peek ahead to detect ".." after an expression
        // Strategy: collect tokens until LOOP, looking for DotDot
        let saved_pos = self.pos;
        let range_body = self.collect_until_loop();

        // Check if range body contains ".."
        let has_dotdot = range_body.iter().any(|t| matches!(t, Token::DotDot));

        if has_dotdot || reverse {
            // Range FOR
            let dotdot_pos = range_body
                .iter()
                .position(|t| matches!(t, Token::DotDot))
                .unwrap_or(range_body.len());

            let from_tokens = &range_body[..dotdot_pos];
            let after_dotdot = if dotdot_pos + 1 < range_body.len() {
                &range_body[dotdot_pos + 1..]
            } else {
                &[]
            };

            // Check for BY
            let by_pos = after_dotdot
                .iter()
                .position(|t| matches!(t, Token::Keyword(Keyword::By)));
            let (to_tokens, by_tokens) = if let Some(bp) = by_pos {
                (&after_dotdot[..bp], Some(&after_dotdot[bp + 1..]))
            } else {
                (after_dotdot, None)
            };

            let from_str = self.tokens_to_str(from_tokens);
            let to_str = self.tokens_to_str(to_tokens);
            let by_str = by_tokens.map(|b| self.tokens_to_str(b));

            let from = self.expr_fragment_to_plexpr(&from_str)?;
            let to = self.expr_fragment_to_plexpr(&to_str)?;
            let by = by_str
                .map(|s| self.expr_fragment_to_plexpr(&s))
                .transpose()?;

            self.expect_keyword(Keyword::Loop)?;
            let stmts = self.parse_statements()?;
            self.expect_keyword(Keyword::End)?;
            self.expect_keyword(Keyword::Loop)?;
            self.eat_token(&Token::Semi);

            Ok(PlStatement::ForRange {
                var,
                reverse,
                from,
                to,
                by,
                body: Block {
                    label: None,
                    statements: stmts,
                    exception: None,
                },
            })
        } else {
            // Query FOR — re-parse the body as a SQL query
            let query_str = self.tokens_to_str(&range_body);
            self.expect_keyword(Keyword::Loop)?;
            let stmts = self.parse_statements()?;
            self.expect_keyword(Keyword::End)?;
            self.expect_keyword(Keyword::Loop)?;
            self.eat_token(&Token::Semi);

            match self.parse_sql_fragment(&query_str, saved_pos) {
                Ok(pgsql_ast::Statement::Select(sel)) => Ok(PlStatement::ForSelect {
                    var,
                    query: sel,
                    body: Block {
                        label: None,
                        statements: stmts,
                        exception: None,
                    },
                }),
                _ => Err(PlParseError::Unsupported(format!(
                    "FOR IN: cannot parse query: {}",
                    &query_str[..query_str.len().min(60)]
                ))),
            }
        }
    }

    /// Collect tokens until LOOP keyword (not consuming it). Returns them.
    fn collect_until_loop(&mut self) -> Vec<Token> {
        let mut result = Vec::new();
        let mut depth = 0i32;
        loop {
            match self.peek() {
                Token::Eof => break,
                Token::Keyword(Keyword::Loop) if depth == 0 => break,
                Token::LParen => {
                    depth += 1;
                    result.push(self.advance());
                }
                Token::RParen => {
                    depth -= 1;
                    result.push(self.advance());
                }
                _ => result.push(self.advance()),
            }
        }
        result
    }

    fn tokens_to_str(&self, tokens: &[Token]) -> String {
        tokens
            .iter()
            .map(|t| self.token_to_str(t))
            .collect::<Vec<_>>()
            .join(" ")
    }

    // ── EXIT / CONTINUE ───────────────────────────────────────────────────

    fn parse_exit_continue(&mut self, is_exit: bool) -> Result<PlStatement, PlParseError> {
        if is_exit {
            self.expect_keyword(Keyword::Exit)?;
        } else {
            self.expect_keyword(Keyword::Continue)?;
        }

        // Optional label
        let label = match self.peek() {
            Token::Ident(_) => {
                // Check it's not WHEN
                if let Token::Ident(s) = self.peek().clone() {
                    if s.to_uppercase() != "WHEN" {
                        self.advance();
                        Some(s)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        };

        // Optional WHEN condition
        let when = if self.eat_keyword(Keyword::When) {
            Some(self.parse_plexpr_until_semi()?)
        } else {
            None
        };

        self.eat_token(&Token::Semi);
        if is_exit {
            Ok(PlStatement::Exit { label, when })
        } else {
            Ok(PlStatement::Continue { label, when })
        }
    }

    // ── EXECUTE ───────────────────────────────────────────────────────────

    fn parse_execute(&mut self) -> Result<PlStatement, PlParseError> {
        self.expect_keyword(Keyword::Execute)?;
        let sql_expr = self.parse_plexpr_atom()?;

        // Optional INTO
        let into = if self.eat_keyword(Keyword::Into) {
            let mut targets = Vec::new();
            loop {
                let name = self.expect_ident()?;
                targets.push(Ident::new(name));
                if !self.eat_token(&Token::Comma) {
                    break;
                }
            }
            targets
        } else {
            vec![]
        };

        // Optional USING
        let using = if self.eat_keyword(Keyword::Using) {
            let mut args = Vec::new();
            loop {
                args.push(self.parse_plexpr_atom()?);
                if !self.eat_token(&Token::Comma) {
                    break;
                }
            }
            args
        } else {
            vec![]
        };

        self.eat_token(&Token::Semi);
        Ok(PlStatement::Execute {
            sql: sql_expr,
            into,
            using,
        })
    }

    // ── CALL ──────────────────────────────────────────────────────────────

    fn parse_call(&mut self) -> Result<PlStatement, PlParseError> {
        self.expect_keyword(Keyword::Call)?;
        let name_str = self.expect_ident()?;
        let name = pgsql_ast::expr::ObjectName(vec![Ident::new(name_str)]);

        let mut args = Vec::new();
        if self.eat_token(&Token::LParen) {
            while !matches!(self.peek(), Token::RParen | Token::Eof) {
                args.push(self.parse_plexpr_atom()?);
                if !self.eat_token(&Token::Comma) {
                    break;
                }
            }
            self.eat_token(&Token::RParen);
        }
        self.eat_token(&Token::Semi);
        Ok(PlStatement::Call { name, args })
    }

    // ── GET DIAGNOSTICS ───────────────────────────────────────────────────

    fn parse_get_diagnostics(&mut self) -> Result<PlStatement, PlParseError> {
        self.expect_keyword(Keyword::Get)?;
        self.expect_keyword(Keyword::Diagnostics)?;

        let mut assignments = Vec::new();
        loop {
            let var_name = self.expect_ident()?;
            self.eat_token(&Token::Eq);
            let item = match self.peek().clone() {
                Token::Ident(s) => {
                    let item = match s.to_uppercase().as_str() {
                        "ROW_COUNT" => DiagnosticsItem::RowCount,
                        "PG_CONTEXT" => DiagnosticsItem::PgContext,
                        "PG_EXCEPTION_CONTEXT" => DiagnosticsItem::PgExceptionContext,
                        "PG_EXCEPTION_DETAIL" => DiagnosticsItem::PgExceptionDetail,
                        "PG_EXCEPTION_HINT" => DiagnosticsItem::PgExceptionHint,
                        "RETURNED_SQLSTATE" => DiagnosticsItem::ReturnedSqlstate,
                        "COLUMN_NAME" => DiagnosticsItem::ColumnName,
                        "CONSTRAINT_NAME" => DiagnosticsItem::ConstraintName,
                        "PG_DATATYPE_NAME" => DiagnosticsItem::PgDatatype,
                        "MESSAGE_TEXT" => DiagnosticsItem::MessageText,
                        "TABLE_NAME" => DiagnosticsItem::TableName,
                        "SCHEMA_NAME" => DiagnosticsItem::SchemaName,
                        _ => DiagnosticsItem::RowCount,
                    };
                    self.advance();
                    item
                }
                _ => {
                    self.advance();
                    DiagnosticsItem::RowCount
                }
            };
            assignments.push(DiagnosticsAssignment {
                var: Ident::new(var_name),
                item,
            });
            if !self.eat_token(&Token::Comma) {
                break;
            }
        }
        self.eat_token(&Token::Semi);
        Ok(PlStatement::GetDiagnostics(assignments))
    }

    // ── Exception block ───────────────────────────────────────────────────

    fn parse_exception_block(&mut self) -> Result<Option<ExceptionBlock>, PlParseError> {
        if !self.eat_keyword(Keyword::Exception) {
            return Ok(None);
        }
        let mut handlers = Vec::new();
        while self.eat_keyword(Keyword::When) {
            let mut conditions = Vec::new();
            loop {
                let cond = match self.peek().clone() {
                    Token::Keyword(Keyword::Others) => {
                        self.advance();
                        ExceptionCondition::Others
                    }
                    Token::Keyword(Keyword::Sqlstate) => {
                        self.advance();
                        let code = self.expect_string_lit()?;
                        ExceptionCondition::SqlState(code)
                    }
                    Token::Ident(s) => {
                        let s = s.clone();
                        self.advance();
                        ExceptionCondition::Named(s)
                    }
                    Token::Keyword(k) => {
                        let s = format!("{:?}", k).to_lowercase();
                        self.advance();
                        ExceptionCondition::Named(s)
                    }
                    _ => break,
                };
                conditions.push(cond);
                // OR between conditions
                if !self.eat_keyword_ident("OR") {
                    break;
                }
            }
            self.expect_keyword(Keyword::Then)?;
            let stmts = self.parse_statements()?;
            handlers.push(ExceptionHandler {
                conditions,
                block: Block {
                    label: None,
                    statements: stmts,
                    exception: None,
                },
            });
        }
        Ok(Some(ExceptionBlock { handlers }))
    }

    // ── Expression helpers ────────────────────────────────────────────────

    fn parse_plexpr_until_keyword(&mut self, stop: Keyword) -> Result<PlExpr, PlParseError> {
        let fragment = self.extract_until_keyword(stop);
        self.expr_fragment_to_plexpr(&fragment)
    }

    fn parse_plexpr_until_semi(&mut self) -> Result<PlExpr, PlParseError> {
        let fragment = self.extract_until_semi_balanced();
        self.expr_fragment_to_plexpr(&fragment)
    }

    /// Parse a single expression atom (for RAISE args, EXECUTE args, etc.)
    fn parse_plexpr_atom(&mut self) -> Result<PlExpr, PlParseError> {
        // Collect until comma, semicolon, or closing paren at depth 0
        let mut parts = Vec::new();
        let mut depth = 0i32;
        loop {
            match self.peek() {
                Token::Eof => break,
                Token::Semi if depth == 0 => break,
                Token::Comma if depth == 0 => break,
                Token::RParen if depth == 0 => break,
                Token::LParen => {
                    depth += 1;
                    parts.push(self.token_to_str_consume());
                }
                Token::RParen => {
                    depth -= 1;
                    parts.push(self.token_to_str_consume());
                }
                // Stop on USING keyword at depth 0
                Token::Keyword(Keyword::Using) if depth == 0 => break,
                _ => parts.push(self.token_to_str_consume()),
            }
        }
        let fragment = parts.join(" ");
        self.expr_fragment_to_plexpr(&fragment)
    }

    fn expr_fragment_to_plexpr(&self, fragment: &str) -> Result<PlExpr, PlParseError> {
        let trimmed = fragment.trim();
        if trimmed.is_empty() {
            return Ok(PlExpr::Sql(Expr::bare(ExprKind::Literal(
                pgsql_ast::expr::Literal::Null,
            ))));
        }

        let upper = trimmed.to_uppercase();

        // Special variable: FOUND
        if upper == "FOUND" {
            return Ok(PlExpr::Found);
        }

        // NOT FOUND
        if upper == "NOT FOUND" {
            return Ok(PlExpr::Not(Box::new(PlExpr::Found)));
        }

        // `NEW.col` / `OLD.col` SEULS — le fragment ne doit rien contenir de plus.
        //
        // Ce raccourci tronquait l'expression entière : il se déclenchait sur
        // tout fragment COMMENÇANT par `NEW.`, prenait le champ, et jetait le
        // reste. `IF OLD.status = 'cancelled' AND NEW.status != 'cancelled'`
        // devenait `OldField("status")` — la comparaison, le AND et toute la
        // clause NEW disparaissaient, en silence, avec un corps `Parsed`.
        // `examples/schema.sql` en contenait trois occurrences, et le test dédié
        // était vert parce qu'il n'assertait que le compte de corps parsés.
        //
        // Une expression plus large est désormais confiée à sqlparser, qui lit
        // `NEW.status` comme une simple colonne qualifiée. Le checker enregistre
        // déjà `new` et `old` comme alias de la table du trigger, donc la
        // résolution fonctionne.
        if let Some(field) = row_var_field(trimmed, "NEW") {
            return Ok(PlExpr::NewField(Ident::new(field)));
        }
        if let Some(field) = row_var_field(trimmed, "OLD") {
            return Ok(PlExpr::OldField(Ident::new(field)));
        }

        // Detect patterns that cannot be PL/pgSQL variable names.
        // A variable name is always a simple identifier (no spaces, no parens,
        // no SQL operators). Anything more complex must be an opaque SQL expr.
        let is_complex = upper.contains(' ')
            || upper.contains('(')
            || upper.contains(')')
            || upper.starts_with("NOT ")
            || upper.starts_with("EXISTS ")
            || upper.contains(" AND ")
            || upper.contains(" OR ")
            || upper.contains(" = ")
            || upper.contains(" != ")
            || upper.contains(" <> ")
            || upper.contains(" IS ");

        // Try to parse via sqlparser as a SELECT expression
        let sql = format!("SELECT {}", trimmed);
        if let Ok(stmts) = crate::parse(&sql)
            && !stmts.is_empty()
            && let pgsql_ast::Statement::Select(ref sel) = stmts[0]
            && let Some(pgsql_ast::stmt::SelectItem::Expr { expr, .. }) = sel.columns.first()
        {
            return Ok(PlExpr::Sql(expr.clone()));
        }

        // Fragment que sqlparser refuse, et qui n'est pas un identifiant simple.
        //
        // Ce bras renvoyait le littéral `true`. Une condition `IF` devenait donc
        // littéralement vraie dans l'AST : plus grave qu'une information perdue,
        // c'était une information fausse, et le commentaire d'origine l'assumait
        // (« so the checker never raises W005 »).
        //
        // `ExprKind::Unsupported` dit ce qui est : une expression que nous
        // n'avons pas su lire. Le corps continue de parser — donc les autres
        // statements restent vérifiés — et rien n'affirme de travers.
        if is_complex {
            return Ok(PlExpr::Sql(Expr::bare(ExprKind::Unsupported(format!(
                "fragment PL/pgSQL : {trimmed}"
            )))));
        }

        // Fallback: treat as a variable reference
        Ok(PlExpr::Var(Ident::new(trimmed)))
    }

    // ── Token extraction helpers ──────────────────────────────────────────

    fn extract_until_semi_balanced(&mut self) -> String {
        let mut depth = 0i32;
        let mut parts = Vec::new();
        loop {
            match self.peek() {
                Token::Eof => break,
                Token::Semi if depth == 0 => break,
                Token::LParen => {
                    depth += 1;
                    parts.push(self.token_to_str_consume());
                }
                Token::RParen => {
                    depth -= 1;
                    if depth < 0 {
                        break;
                    }
                    parts.push(self.token_to_str_consume());
                }
                _ => {
                    parts.push(self.token_to_str_consume());
                }
            }
        }
        parts.join(" ")
    }

    fn extract_until_keyword(&mut self, stop: Keyword) -> String {
        let mut depth = 0i32;
        let mut parts = Vec::new();
        loop {
            match self.peek() {
                Token::Eof => break,
                Token::Keyword(k) if *k == stop && depth == 0 => break,
                Token::LParen => {
                    depth += 1;
                    parts.push(self.token_to_str_consume());
                }
                Token::RParen => {
                    depth -= 1;
                    parts.push(self.token_to_str_consume());
                }
                _ => parts.push(self.token_to_str_consume()),
            }
        }
        parts.join(" ")
    }

    /// Consume the current token and return its string representation.
    fn token_to_str_consume(&mut self) -> String {
        let t = self.advance();
        self.token_to_str(&t)
    }

    fn token_to_str(&self, t: &Token) -> String {
        match t {
            Token::Keyword(k) => keyword_to_sql_str(k),
            Token::Ident(s) => s.clone(),
            Token::Number(s) => s.clone(),
            // Rendu verbatim : c'est tout l'intérêt du token.
            Token::Op(s) => s.clone(),
            Token::StringLit(s) => format!("'{}'", s.replace('\'', "''")),
            Token::Semi => ";".to_string(),
            Token::Comma => ",".to_string(),
            Token::Dot => ".".to_string(),
            Token::ColonEq => ":=".to_string(),
            Token::Assign => "=".to_string(),
            Token::Eq => "=".to_string(),
            Token::NotEq => "!=".to_string(),
            Token::Lt => "<".to_string(),
            Token::Lte => "<=".to_string(),
            Token::Gt => ">".to_string(),
            Token::Gte => ">=".to_string(),
            Token::Plus => "+".to_string(),
            Token::Minus => "-".to_string(),
            Token::Star => "*".to_string(),
            Token::Slash => "/".to_string(),
            Token::Percent => "%".to_string(),
            Token::LParen => "(".to_string(),
            Token::RParen => ")".to_string(),
            Token::LBracket => "[".to_string(),
            Token::RBracket => "]".to_string(),
            Token::DotDot => "..".to_string(),
            Token::Colon => ":".to_string(),
            Token::Percent2 => "%".to_string(),
            Token::DollarLabel(s) => format!("${}$", s),
            Token::Eof => "".to_string(),
        }
    }

    // ── SQL fragment parser ───────────────────────────────────────────────

    /// Parse a SQL fragment string. `token_start` is the `self.pos` value
    /// captured immediately before the extraction that produced `sql`; it's
    /// used to compute the fragment's byte offset within the source file so
    /// that diagnostic spans point to the correct line.
    fn parse_sql_fragment(
        &self,
        sql: &str,
        token_start: usize,
    ) -> Result<pgsql_ast::Statement, PlParseError> {
        // Position du fragment dans le document, avant de parser — et non un
        // décalage appliqué après.
        //
        // `shift_stmt_span` ne corrigeait que le span du statement, jamais ceux
        // des expressions imbriquées, qui restaient relatifs au fragment et
        // fuyaient tels quels : un `WHERE col_inexistante` dans un corps de
        // fonction était rapporté sur la première ligne du fichier. En parsant
        // à la bonne position, tous les spans sont justes d'emblée.
        let leading = sql.len() - sql.trim_start().len();
        let trimmed = sql.trim().trim_end_matches(';');
        let full = format!("{};", trimmed);

        let body_offset = self.token_offsets.get(token_start).copied().unwrap_or(0);
        // Le début du fragment tombe dans un fragment recopié, donc son
        // antécédent est exact même sous une correspondance non affine.
        let file_offset = self
            .offsets
            .point(self.file_body_offset + body_offset + leading);

        match crate::parse_at(&full, file_offset) {
            Ok(stmts) if !stmts.is_empty() => Ok(stmts.into_iter().next().unwrap()),
            Ok(_) => Err(PlParseError::Unsupported("empty parse result".to_string())),
            Err(e) => Err(PlParseError::Unsupported(e.to_string())),
        }
    }
}

// ── Free functions ──────────────────────────────────────────────────────────

/// Strip ` INTO <var>` that follows a `RETURNING` clause in PL/pgSQL DML.
///
/// `RETURNING cols INTO v_var` is valid PL/pgSQL but not standard SQL.
/// sqlparser only understands `RETURNING cols`, so we strip the ` INTO var`
/// tail before parsing. The variable capture is discarded — we only need the
/// statement structure for type-checking, not the assignment target.
fn strip_returning_into(fragment: &str) -> &str {
    let upper = fragment.to_uppercase();
    // Find RETURNING keyword position (last occurrence to skip INSERT INTO / FROM clauses)
    let Some(ret_pos) = upper.rfind("RETURNING") else {
        return fragment;
    };
    // Look for INTO after RETURNING
    let after_ret = &upper[ret_pos..];
    // Find `INTO` that is at word boundary and not preceded by INSERT
    let mut search = ret_pos;
    while let Some(rel) = upper[search..].find(" INTO ") {
        let abs = search + rel;
        // Everything before should be after RETURNING
        if abs >= ret_pos {
            return fragment[..abs].trim_end();
        }
        search = abs + 6;
    }
    // Also check for `INTO` at end of string (without trailing space)
    let _ = after_ret; // suppress unused warning
    fragment
}

/// Find the position of " INTO " in an uppercase string, not as part of
/// "INSERT INTO".
fn find_into_position(upper: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(pos) = upper[search_from..].find(" INTO ") {
        let abs = search_from + pos;
        // Make sure it's not preceded by "INSERT"
        let before = upper[..abs].trim_end();
        if before.ends_with("INSERT") {
            search_from = abs + 6;
            continue;
        }
        return Some(abs);
    }
    // Also try at the start of the string
    if upper.starts_with("INTO ") {
        return Some(0);
    }
    None
}

fn keyword_to_sql_str(k: &Keyword) -> String {
    match k {
        Keyword::Begin => "BEGIN",
        Keyword::End => "END",
        Keyword::Declare => "DECLARE",
        Keyword::If => "IF",
        Keyword::Then => "THEN",
        Keyword::Elsif => "ELSIF",
        Keyword::Else => "ELSE",
        Keyword::EndIf => "END IF",
        Keyword::Loop => "LOOP",
        Keyword::EndLoop => "END LOOP",
        Keyword::While => "WHILE",
        Keyword::For => "FOR",
        Keyword::In => "IN",
        Keyword::Reverse => "REVERSE",
        Keyword::By => "BY",
        Keyword::Exit => "EXIT",
        Keyword::Continue => "CONTINUE",
        Keyword::When => "WHEN",
        Keyword::Return => "RETURN",
        Keyword::ReturnNext => "NEXT",
        Keyword::ReturnQuery => "QUERY",
        Keyword::Select => "SELECT",
        Keyword::Insert => "INSERT",
        Keyword::Update => "UPDATE",
        Keyword::Delete => "DELETE",
        Keyword::Perform => "PERFORM",
        Keyword::Execute => "EXECUTE",
        Keyword::Call => "CALL",
        Keyword::Into => "INTO",
        Keyword::Strict => "STRICT",
        Keyword::Using => "USING",
        Keyword::Raise => "RAISE",
        Keyword::Exception => "EXCEPTION",
        Keyword::Debug => "DEBUG",
        Keyword::Log => "LOG",
        Keyword::Info => "INFO",
        Keyword::Notice => "NOTICE",
        Keyword::Warning => "WARNING",
        Keyword::Message => "MESSAGE",
        Keyword::Detail => "DETAIL",
        Keyword::Hint => "HINT",
        Keyword::Errcode => "ERRCODE",
        Keyword::Column => "COLUMN",
        Keyword::Constraint => "CONSTRAINT",
        Keyword::Datatype => "DATATYPE",
        Keyword::Table => "TABLE",
        Keyword::Schema => "SCHEMA",
        Keyword::Get => "GET",
        Keyword::Diagnostics => "DIAGNOSTICS",
        Keyword::Null => "NULL",
        Keyword::Not => "NOT",
        Keyword::And => "AND",
        Keyword::Or => "OR",
        Keyword::Found => "FOUND",
        Keyword::New => "NEW",
        Keyword::Old => "OLD",
        Keyword::Sqlstate => "SQLSTATE",
        Keyword::Others => "OTHERS",
        Keyword::No => "NO",
        Keyword::Other => "OTHER",
    }
    .to_string()
}

/// Champ d'une variable ligne, si le fragment est **exactement** `VAR.champ`.
///
/// Renvoie `None` dès que le fragment contient quoi que ce soit d'autre : c'est
/// la condition qui évite de tronquer une expression plus large.
fn row_var_field(fragment: &str, var: &str) -> Option<String> {
    let t = fragment.trim();
    let rest = t
        .strip_prefix(var)
        .or_else(|| t.strip_prefix(&var.to_lowercase()))?;
    let rest = rest.trim_start().strip_prefix('.')?.trim_start();

    let field: String = rest
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '_')
        .collect();
    if field.is_empty() {
        return None;
    }
    // Rien ne doit suivre le champ : sinon l'expression est plus large que
    // `VAR.champ` et doit être parsée entièrement.
    if rest[field.len()..].trim().is_empty() {
        Some(field)
    } else {
        None
    }
}
