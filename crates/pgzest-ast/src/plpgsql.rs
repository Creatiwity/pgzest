//! PL/pgSQL AST.
//!
//! Expressions are ordinary SQL [`Expr`]s: in PL/pgSQL, an expression
//! identifier can denote a variable, a parameter, a column or a special
//! variable, and only semantic analysis — which knows the scope — can
//! decide. The parser does not guess.

use crate::expr::{Expr, Ident, ObjectName};
use crate::node::Span;
use crate::stmt::{SelectStmt, Statement};
use crate::types::DataType;
use serde::{Deserialize, Serialize};

// ── Body ──────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlpgsqlBody {
    /// Main block, with its declarations.
    pub block: Block,
    /// Body text as written, between its delimiters. This is what SQL
    /// emission restores: regenerating the PL/pgSQL from the AST would lose
    /// formatting and comments, and a migration must deploy the body the
    /// developer wrote.
    pub source: String,
}

/// `[<<label>>] [DECLARE …] BEGIN … [EXCEPTION …] END [label]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub label: Option<String>,
    /// Declarations of this block, visible in this block and its nested blocks.
    pub declarations: Vec<Declaration>,
    pub statements: Vec<PlStatement>,
    pub exception: Option<ExceptionBlock>,
}

// ── Declarations ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Declaration {
    Variable(VarDecl),
    /// `name ALIAS FOR $1` / `ALIAS FOR other`.
    Alias {
        name: Ident,
        target: String,
    },
    /// `name CURSOR [(parameters)] FOR query`.
    Cursor {
        name: Ident,
        params: Vec<(Ident, DataType)>,
        query: Box<SelectStmt>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VarDecl {
    pub name: Ident,
    pub var_type: VarType,
    pub constant: bool,
    pub not_null: bool,
    pub default: Option<Expr>,
    pub span: Span,
}

/// Type of a PL/pgSQL variable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VarType {
    Data(DataType),
    /// `table%ROWTYPE`: a row of the table.
    RowTypeOf(ObjectName),
    /// `table.column%TYPE` or `variable%TYPE`.
    TypeOf(ObjectName),
    /// `RECORD`: structure fixed by the first assignment.
    Record,
}

// ── Exceptions ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExceptionBlock {
    pub handlers: Vec<ExceptionHandler>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExceptionHandler {
    pub conditions: Vec<ExceptionCondition>,
    pub statements: Vec<PlStatement>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExceptionCondition {
    SqlState(String), // SQLSTATE '22012'
    Named(String),    // division_by_zero, unique_violation, etc.
    Others,           // WHEN OTHERS
}

// ── Statements ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlStatement {
    pub span: Span,
    pub kind: PlStatementKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PlStatementKind {
    /// `target := expr` (or `=`). The target is a path: `v`, `rec.field`,
    /// `NEW.column`.
    Assign {
        target: Vec<Ident>,
        value: Expr,
    },
    /// SQL statement, with its optional `INTO` targets (`SELECT … INTO`,
    /// `INSERT … RETURNING … INTO`).
    Sql {
        statement: Statement,
        into: Option<IntoTarget>,
    },
    /// `PERFORM query`: executes and discards the result.
    Perform(SelectStmt),
    /// `EXECUTE string [INTO …] [USING …]`: dynamic SQL, not analyzable.
    Execute {
        sql: Expr,
        into: Option<IntoTarget>,
        using: Vec<Expr>,
    },
    Return(Option<Expr>),
    ReturnNext(Option<Expr>),
    ReturnQuery(SelectStmt),
    ReturnQueryExecute {
        sql: Expr,
        using: Vec<Expr>,
    },
    Raise {
        level: RaiseLevel,
        /// Format string (`'… % …'`), or condition name / SQLSTATE.
        message: Option<String>,
        args: Vec<Expr>,
        options: Vec<RaiseOption>,
    },
    Assert {
        condition: Expr,
        message: Option<Expr>,
    },
    If {
        condition: Expr,
        then_branch: Vec<PlStatement>,
        elsif_branches: Vec<(Expr, Vec<PlStatement>)>,
        else_branch: Option<Vec<PlStatement>>,
    },
    /// `CASE [operand] WHEN values THEN … [ELSE …] END CASE`.
    Case {
        operand: Option<Expr>,
        whens: Vec<(Vec<Expr>, Vec<PlStatement>)>,
        else_branch: Option<Vec<PlStatement>>,
    },
    Loop {
        label: Option<String>,
        body: Vec<PlStatement>,
    },
    While {
        label: Option<String>,
        condition: Expr,
        body: Vec<PlStatement>,
    },
    /// `FOR i IN [REVERSE] a .. b [BY step] LOOP`.
    ForRange {
        label: Option<String>,
        var: Ident,
        reverse: bool,
        from: Expr,
        to: Expr,
        by: Option<Expr>,
        body: Vec<PlStatement>,
    },
    /// `FOR targets IN query LOOP`.
    ForQuery {
        label: Option<String>,
        targets: Vec<Ident>,
        query: SelectStmt,
        body: Vec<PlStatement>,
    },
    /// `FOR targets IN EXECUTE string [USING …] LOOP`.
    ForExecute {
        label: Option<String>,
        targets: Vec<Ident>,
        sql: Expr,
        using: Vec<Expr>,
        body: Vec<PlStatement>,
    },
    /// `FOR rec IN cursor [(arguments)] LOOP`.
    ForCursor {
        label: Option<String>,
        var: Ident,
        cursor: Ident,
        args: Vec<Expr>,
        body: Vec<PlStatement>,
    },
    /// `FOREACH targets [SLICE n] IN ARRAY array LOOP`.
    Foreach {
        label: Option<String>,
        targets: Vec<Ident>,
        slice: Option<u32>,
        array: Expr,
        body: Vec<PlStatement>,
    },
    Exit {
        label: Option<String>,
        when: Option<Expr>,
    },
    Continue {
        label: Option<String>,
        when: Option<Expr>,
    },
    GetDiagnostics {
        stacked: bool,
        items: Vec<DiagnosticsAssignment>,
    },
    /// `OPEN cursor [(arguments)]` or `OPEN cursor FOR query`.
    Open {
        cursor: Ident,
        args: Vec<Expr>,
        query: Option<SelectStmt>,
    },
    /// `FETCH [direction FROM] cursor INTO targets`.
    Fetch {
        cursor: Ident,
        into: IntoTarget,
    },
    Close(Ident),
    /// Nested block, with its own declarations.
    Block(Block),
    Null,
}

/// Targets of an `INTO`: each is a path (`v`, `rec.field`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntoTarget {
    /// `INTO STRICT`: exactly one row expected.
    pub strict: bool,
    pub targets: Vec<Vec<Ident>>,
}

// ── RAISE ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RaiseLevel {
    Debug,
    Log,
    Info,
    Notice,
    Warning,
    Exception,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RaiseOption {
    pub key: RaiseOptionKey,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RaiseOptionKey {
    Message,
    Detail,
    Hint,
    Errcode,
    Column,
    Constraint,
    Datatype,
    Table,
    Schema,
}

// ── GET DIAGNOSTICS ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticsAssignment {
    pub var: Ident,
    pub item: DiagnosticsItem,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DiagnosticsItem {
    RowCount,
    PgContext,
    PgRoutineOid,
    PgExceptionContext,
    PgExceptionDetail,
    PgExceptionHint,
    ReturnedSqlstate,
    ColumnName,
    ConstraintName,
    PgDatatypeName,
    MessageText,
    TableName,
    SchemaName,
}
