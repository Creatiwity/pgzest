use crate::expr::{Expr, Ident, ObjectName};
use crate::stmt::SelectStmt;
use crate::types::DataType;
use serde::{Deserialize, Serialize};

// ── Corps complet d'une fonction/procédure PL/pgSQL ──────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlpgsqlBody {
    pub declare: Vec<VarDecl>,
    pub block: Block,
}

// ── DECLARE ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VarDecl {
    pub name: Ident,
    pub data_type: DataType,
    pub not_null: bool,
    pub constant: bool,
    pub default: Option<PlExpr>,
}

// ── Bloc BEGIN … END ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub label: Option<String>,
    pub statements: Vec<PlStatement>,
    pub exception: Option<ExceptionBlock>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExceptionBlock {
    pub handlers: Vec<ExceptionHandler>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExceptionHandler {
    pub conditions: Vec<ExceptionCondition>,
    pub block: Block,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExceptionCondition {
    SqlState(String), // SQLSTATE '22012'
    Named(String),    // division_by_zero, unique_violation, etc.
    Others,           // WHEN OTHERS
}

// ── Statements PL/pgSQL ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PlStatement {
    /// var := expr  ou  var = expr (dans les triggers)
    Assign { target: AssignTarget, value: PlExpr },
    /// SELECT … INTO [STRICT] var1, var2 FROM …
    SelectInto {
        strict: bool,
        targets: Vec<Ident>,
        query: SelectStmt,
    },
    /// PERFORM expression (exécute et jette le résultat)
    Perform(PlExpr),
    /// EXECUTE sql [INTO var] [USING expr, …]
    Execute {
        sql: PlExpr,
        into: Vec<Ident>,
        using: Vec<PlExpr>,
    },
    /// RETURN [expr]
    Return(Option<PlExpr>),
    /// RETURN NEXT expr
    ReturnNext(PlExpr),
    /// RETURN QUERY query
    ReturnQuery(SelectStmt),
    /// RAISE [level] [format | USING …]
    Raise {
        level: RaiseLevel,
        message: Option<String>,
        args: Vec<PlExpr>,
        options: Vec<RaiseOption>,
    },
    /// IF condition THEN … [ELSIF … THEN …] [ELSE …] END IF
    If {
        condition: PlExpr,
        then_block: Block,
        elsif_clauses: Vec<ElsIf>,
        else_block: Option<Block>,
    },
    /// LOOP … END LOOP
    Loop(Block),
    /// WHILE condition LOOP … END LOOP
    While { condition: PlExpr, body: Block },
    /// FOR var IN [REVERSE] from .. to [BY step] LOOP … END LOOP
    ForRange {
        var: Ident,
        reverse: bool,
        from: PlExpr,
        to: PlExpr,
        by: Option<PlExpr>,
        body: Block,
    },
    /// FOR var IN query LOOP … END LOOP
    ForSelect {
        var: Ident,
        query: SelectStmt,
        body: Block,
    },
    /// FOR var IN EXECUTE sql [USING …] LOOP … END LOOP
    ForExecute {
        var: Ident,
        sql: PlExpr,
        using: Vec<PlExpr>,
        body: Block,
    },
    /// EXIT [label] [WHEN condition]
    Exit {
        label: Option<String>,
        when: Option<PlExpr>,
    },
    /// CONTINUE [label] [WHEN condition]
    Continue {
        label: Option<String>,
        when: Option<PlExpr>,
    },
    /// Instruction SQL directe (INSERT, UPDATE, DELETE)
    Sql(crate::stmt::Statement),
    /// CALL procedure(args)
    Call { name: ObjectName, args: Vec<PlExpr> },
    /// GET DIAGNOSTICS var = item
    GetDiagnostics(Vec<DiagnosticsAssignment>),
    /// Bloc imbriqué (BEGIN … END à l'intérieur d'un bloc)
    SubBlock(Block),
    /// NULL (no-op)
    Null,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElsIf {
    pub condition: PlExpr,
    pub block: Block,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DiagnosticsAssignment {
    pub var: Ident,
    pub item: DiagnosticsItem,
}

// ── Cible d'assignation ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AssignTarget {
    /// Variable simple : `v_count`
    Simple(Ident),
    /// Champ de record : `rec.field`
    Field(Ident, Ident),
    /// NEW.col dans un trigger
    NewField(Ident),
    OldField(Ident),
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
    pub value: PlExpr,
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
pub enum DiagnosticsItem {
    RowCount,
    PgContext,
    PgExceptionContext,
    PgExceptionDetail,
    PgExceptionHint,
    ReturnedSqlstate,
    ColumnName,
    ConstraintName,
    PgDatatype,
    MessageText,
    TableName,
    SchemaName,
}

// ── Expression PL/pgSQL ───────────────────────────────────────────────────────

/// Superset d'Expr SQL : ajoute les variables locales et les spéciales PL/pgSQL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PlExpr {
    /// Expression SQL standard (colonnes, littéraux, fonctions…)
    Sql(Expr),
    /// Variable locale déclarée dans DECLARE
    Var(Ident),
    /// Champ d'un record : rec.field
    RecordField { record: Ident, field: Ident },
    /// NEW.col dans un trigger
    NewField(Ident),
    /// OLD.col dans un trigger
    OldField(Ident),
    /// Variable spéciale FOUND (true si dernière requête a retourné des lignes)
    Found,
    /// SQLSTATE (dans EXCEPTION handlers)
    Sqlstate,
    /// NOT expr
    Not(Box<PlExpr>),
    /// expr AND expr
    And(Box<PlExpr>, Box<PlExpr>),
    /// expr OR expr
    Or(Box<PlExpr>, Box<PlExpr>),
    /// Comparaison binaire simple (wrapping un ExprKind::BinaryOp)
    BinOp(Box<PlExpr>, String, Box<PlExpr>),
}
