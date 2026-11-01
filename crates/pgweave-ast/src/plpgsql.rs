//! AST du PL/pgSQL.
//!
//! Les expressions sont des [`Expr`] SQL ordinaires : en PL/pgSQL, un
//! identifiant d'expression peut désigner une variable, un paramètre, une
//! colonne ou une variable spéciale, et seule l'analyse sémantique — qui
//! connaît la portée — peut trancher. Le parser ne devine pas.

use crate::expr::{Expr, Ident, ObjectName};
use crate::node::Span;
use crate::stmt::{SelectStmt, Statement};
use crate::types::DataType;
use serde::{Deserialize, Serialize};

// ── Corps ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlpgsqlBody {
    /// Bloc principal, avec ses déclarations.
    pub block: Block,
    /// Texte du corps tel qu'écrit, entre ses délimiteurs. C'est lui que
    /// restitue l'émission SQL : régénérer le PL/pgSQL depuis l'AST perdrait
    /// la mise en forme et les commentaires, et une migration doit déployer
    /// le corps que le développeur a écrit.
    pub source: String,
}

/// `[<<label>>] [DECLARE …] BEGIN … [EXCEPTION …] END [label]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub label: Option<String>,
    /// Déclarations de ce bloc, visibles dans ce bloc et ses blocs imbriqués.
    pub declarations: Vec<Declaration>,
    pub statements: Vec<PlStatement>,
    pub exception: Option<ExceptionBlock>,
}

// ── Déclarations ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Declaration {
    Variable(VarDecl),
    /// `nom ALIAS FOR $1` / `ALIAS FOR autre`.
    Alias {
        name: Ident,
        target: String,
    },
    /// `nom CURSOR [(paramètres)] FOR requête`.
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

/// Type d'une variable PL/pgSQL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum VarType {
    Data(DataType),
    /// `table%ROWTYPE` : une ligne de la table.
    RowTypeOf(ObjectName),
    /// `table.colonne%TYPE` ou `variable%TYPE`.
    TypeOf(ObjectName),
    /// `RECORD` : structure fixée par la première affectation.
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
    /// `cible := expr` (ou `=`). La cible est un chemin : `v`, `rec.champ`,
    /// `NEW.colonne`.
    Assign {
        target: Vec<Ident>,
        value: Expr,
    },
    /// Statement SQL, avec ses cibles `INTO` éventuelles (`SELECT … INTO`,
    /// `INSERT … RETURNING … INTO`).
    Sql {
        statement: Statement,
        into: Option<IntoTarget>,
    },
    /// `PERFORM requête` : exécute et ignore le résultat.
    Perform(SelectStmt),
    /// `EXECUTE chaîne [INTO …] [USING …]` : SQL dynamique, non analysable.
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
        /// Chaîne de format (`'… % …'`), ou nom de condition / SQLSTATE.
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
    /// `CASE [opérande] WHEN valeurs THEN … [ELSE …] END CASE`.
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
    /// `FOR i IN [REVERSE] a .. b [BY pas] LOOP`.
    ForRange {
        label: Option<String>,
        var: Ident,
        reverse: bool,
        from: Expr,
        to: Expr,
        by: Option<Expr>,
        body: Vec<PlStatement>,
    },
    /// `FOR cibles IN requête LOOP`.
    ForQuery {
        label: Option<String>,
        targets: Vec<Ident>,
        query: SelectStmt,
        body: Vec<PlStatement>,
    },
    /// `FOR cibles IN EXECUTE chaîne [USING …] LOOP`.
    ForExecute {
        label: Option<String>,
        targets: Vec<Ident>,
        sql: Expr,
        using: Vec<Expr>,
        body: Vec<PlStatement>,
    },
    /// `FOR rec IN curseur [(arguments)] LOOP`.
    ForCursor {
        label: Option<String>,
        var: Ident,
        cursor: Ident,
        args: Vec<Expr>,
        body: Vec<PlStatement>,
    },
    /// `FOREACH cibles [SLICE n] IN ARRAY tableau LOOP`.
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
    /// `OPEN curseur [(arguments)]` ou `OPEN curseur FOR requête`.
    Open {
        cursor: Ident,
        args: Vec<Expr>,
        query: Option<SelectStmt>,
    },
    /// `FETCH [direction FROM] curseur INTO cibles`.
    Fetch {
        cursor: Ident,
        into: IntoTarget,
    },
    Close(Ident),
    /// Bloc imbriqué, avec ses propres déclarations.
    Block(Block),
    Null,
}

/// Cibles d'un `INTO` : chacune est un chemin (`v`, `rec.champ`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IntoTarget {
    /// `INTO STRICT` : exactement une ligne attendue.
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
