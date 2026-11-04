use crate::expr::{Expr, Ident, ObjectName};
use crate::node::{NodeId, Span};
use crate::types::DataType;
use serde::{Deserialize, Serialize};

// `Statement` is 448 bytes, dominated by `SelectStmt` (240 for Insert).
// Boxing the variants would reduce the size but break pattern matching
// in every consumer crate (checker, lint, codegen, semantic, lsp).
// Statements are handled by reference and in Vecs, never copied in
// bulk: the real cost is low. To reconsider if parsing large
// files becomes a measured hot spot.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Statement {
    Select(SelectStmt),
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    Ddl(crate::ddl::DdlStatement),
    Utility(crate::utility::Utility),
}

// ── SELECT ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectStmt {
    pub id: NodeId,
    pub span: Span,
    pub with: Vec<Cte>,
    /// `WITH RECURSIVE`: CTEs may reference themselves.
    pub with_recursive: bool,
    pub distinct: bool,
    /// `DISTINCT ON (expr, …)`: one row per value of these expressions
    /// (implies `distinct`).
    pub distinct_on: Vec<Expr>,
    pub columns: Vec<SelectItem>,
    pub from: Vec<TableRef>,
    pub joins: Vec<Join>,
    pub where_clause: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    /// `WINDOW name AS (…)` clause, in writing order.
    pub windows: Vec<NamedWindow>,
    pub order_by: Vec<OrderByExpr>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
    /// `UNION` / `INTERSECT` / `EXCEPT` following this SELECT, applied left
    /// to right; each operand may carry its own operations
    /// (`a UNION b INTERSECT c` = `a UNION (b INTERSECT c)`). The result
    /// columns are named by this first SELECT; `order_by`, `limit` and
    /// `offset` then apply to the whole.
    pub set_ops: Vec<SetOperation>,
    /// `VALUES (…), (…)`: body made of value rows, columns named
    /// `column1`, `column2`… The `SELECT`-specific clauses are then
    /// empty; `order_by`, `limit`, `offset` and `set_ops` apply.
    pub values: Vec<Vec<Expr>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetOperation {
    pub op: SetOperator,
    /// `ALL`: keeps duplicates.
    pub all: bool,
    pub select: SelectStmt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetOperator {
    Union,
    Intersect,
    Except,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cte {
    pub name: Ident,
    pub columns: Vec<Ident>,
    pub query: CteQuery,
}

/// Body of a CTE: a query, or a data-modifying statement whose
/// `RETURNING` forms the rows (`WITH n AS (INSERT … RETURNING id)`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CteQuery {
    Select(Box<SelectStmt>),
    /// `INSERT`, `UPDATE` or `DELETE`.
    Modify(Box<Statement>),
}

impl Cte {
    /// Body query, if it is one.
    pub fn select(&self) -> Option<&SelectStmt> {
        match &self.query {
            CteQuery::Select(q) => Some(q),
            CteQuery::Modify(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SelectItem {
    Wildcard,
    QualifiedWildcard(Ident),
    Expr { expr: Expr, alias: Option<Ident> },
}

/// Table modified by `UPDATE` or `DELETE`: always a named table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetTable {
    pub name: ObjectName,
    pub alias: Option<Ident>,
    /// Position of the name, `DUMMY` if unknown.
    #[serde(default)]
    pub span: Span,
}

/// `FROM` or join item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableRef {
    pub source: TableSource,
    pub alias: Option<Ident>,
    /// `AS alias (c1, c2)`: renames the source's columns, in order.
    pub column_aliases: Vec<Ident>,
    /// Column definition list of a function returning `record`:
    /// `AS r(a int, b text)`. One type per column alias, or empty.
    pub column_types: Vec<DataType>,
    /// Position of the source (the name of a table), `DUMMY` if unknown.
    #[serde(default)]
    pub span: crate::Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TableSource {
    /// Named table, view or CTE.
    Table(ObjectName),
    /// `(SELECT …)`, possibly `LATERAL` (sees the preceding sources).
    Subquery {
        lateral: bool,
        query: Box<SelectStmt>,
    },
    /// Set-returning function: `generate_series(1, 3)`,
    /// `unnest(arr)`, a project function `RETURNS TABLE`.
    Function {
        lateral: bool,
        name: ObjectName,
        args: Vec<Expr>,
        with_ordinality: bool,
    },
}

impl TableRef {
    /// Named table, if the source is one.
    pub fn table_name(&self) -> Option<&ObjectName> {
        match &self.source {
            TableSource::Table(n) => Some(n),
            _ => None,
        }
    }

    /// Name under which the source is visible in the query: its alias,
    /// otherwise the last segment of its name (table or function).
    pub fn visible_name(&self) -> Option<&str> {
        if let Some(a) = &self.alias {
            return Some(&a.value);
        }
        match &self.source {
            TableSource::Table(n) | TableSource::Function { name: n, .. } => {
                n.0.last().map(|i| i.value.as_str())
            }
            TableSource::Subquery { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Join {
    pub kind: JoinKind,
    pub table: TableRef,
    pub constraint: JoinConstraint,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum JoinConstraint {
    On(Expr),
    Using(Vec<Ident>),
    Natural,
    /// `CROSS JOIN` (or comma): Cartesian product, without a condition.
    None,
}

/// Names given to the rows before (`OLD`) and after (`NEW`) modification in
/// the `RETURNING` of a data-modifying statement (PostgreSQL 18). When absent,
/// they are `old` and `new`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ReturningAliases {
    pub old: Option<Ident>,
    pub new: Option<Ident>,
}

impl ReturningAliases {
    pub fn is_empty(&self) -> bool {
        self.old.is_none() && self.new.is_none()
    }
}

/// `WINDOW name AS (specification)`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedWindow {
    pub name: Ident,
    pub spec: crate::expr::WindowSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OrderByExpr {
    pub expr: Expr,
    pub asc: Option<bool>,
    pub nulls_first: Option<bool>,
}

// ── INSERT ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InsertStmt {
    pub id: NodeId,
    pub span: Span,
    /// `WITH` at the head of the statement: CTEs visible to the whole statement.
    pub with: Vec<Cte>,
    pub with_recursive: bool,
    pub table: ObjectName,
    /// Position of the target table's name, `DUMMY` if unknown.
    #[serde(default)]
    pub table_span: Span,
    /// `INSERT INTO t AS alias`: name of the target table in `ON CONFLICT` and
    /// `RETURNING`.
    pub alias: Option<Ident>,
    pub columns: Vec<Ident>,
    pub source: InsertSource,
    pub on_conflict: Option<OnConflict>,
    pub returning: Vec<SelectItem>,
    /// `RETURNING WITH (OLD AS o, NEW AS n)` (PostgreSQL 18).
    pub returning_aliases: ReturningAliases,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Select(Box<SelectStmt>),
    /// `DEFAULT VALUES`: one row, each column at its default value.
    DefaultValues,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OnConflict {
    pub target: OnConflictTarget,
    pub action: OnConflictAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OnConflictTarget {
    Column(Vec<Ident>),
    Constraint(Ident),
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum OnConflictAction {
    DoNothing,
    Update {
        assignments: Vec<Assignment>,
        where_clause: Option<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    /// Assigned columns: a single one (`a = v`), or several for the
    /// multi-column form `(a, b) = (v1, v2)` / `(a, b) = (SELECT …)`.
    pub columns: Vec<Ident>,
    /// Fields of an assigned composite column: `SET home.city = …` gives
    /// `columns = [home]`, `subfields = [city]`. Empty otherwise.
    pub subfields: Vec<Ident>,
    pub value: Expr,
}

// ── UPDATE ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateStmt {
    pub id: NodeId,
    pub span: Span,
    /// `WITH` at the head of the statement: CTEs visible to the whole statement.
    pub with: Vec<Cte>,
    pub with_recursive: bool,
    pub table: TargetTable,
    pub assignments: Vec<Assignment>,
    pub from: Vec<TableRef>,
    /// Joins written in `FROM`.
    pub joins: Vec<Join>,
    pub where_clause: Option<Expr>,
    pub returning: Vec<SelectItem>,
    /// `RETURNING WITH (OLD AS o, NEW AS n)` (PostgreSQL 18).
    pub returning_aliases: ReturningAliases,
}

// ── DELETE ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeleteStmt {
    pub id: NodeId,
    pub span: Span,
    /// `WITH` at the head of the statement: CTEs visible to the whole statement.
    pub with: Vec<Cte>,
    pub with_recursive: bool,
    pub table: TargetTable,
    pub using: Vec<TableRef>,
    /// Joins written in `USING`.
    pub joins: Vec<Join>,
    pub where_clause: Option<Expr>,
    pub returning: Vec<SelectItem>,
    /// `RETURNING WITH (OLD AS o, NEW AS n)` (PostgreSQL 18).
    pub returning_aliases: ReturningAliases,
}

// Suppress unused import warning — DataType is part of the public API surface
// and may be needed by consumers of this module.
const _: Option<DataType> = None;

/// Span of the statement, whatever its kind.
///
/// Only statement-level nodes carry a span: `Expr`, `Ident` and
/// `ColumnRef` do not. This is therefore the finest granularity available
/// today to locate a diagnostic, and the one the type-checker uses.
///
/// Returns `None` for DDL without a span (variants other than
/// CREATE TABLE / FUNCTION / PROCEDURE / POLICY), and for a span left at
/// [`Span::DUMMY`] — a null span would point at 1:1, which we want to avoid.
pub fn stmt_span(stmt: &Statement) -> Option<Span> {
    use crate::ddl::DdlStatement;

    let span = match stmt {
        Statement::Select(s) => s.span,
        Statement::Insert(s) => s.span,
        Statement::Update(s) => s.span,
        Statement::Delete(s) => s.span,
        Statement::Utility(u) => u.span,
        Statement::Ddl(ddl) => match ddl {
            DdlStatement::CreateTable(c) => c.span,
            DdlStatement::CreateFunction(c) => c.span,
            DdlStatement::CreateProcedure(c) => c.span,
            DdlStatement::CreatePolicy(c) => c.span,
            DdlStatement::CreateIndex(c) => c.span,
            _ => return None,
        },
    };

    if span == Span::DUMMY {
        None
    } else {
        Some(span)
    }
}
