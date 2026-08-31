use crate::expr::{Expr, Ident, ObjectName};
use crate::node::{NodeId, Span};
use crate::types::DataType;
use serde::{Deserialize, Serialize};

// `Statement` fait 448 octets, dominé par `SelectStmt` (240 pour Insert).
// Boxer les variants réduirait la taille mais casserait le pattern matching
// dans tous les crates consommateurs (checker, lint, codegen, semantic, lsp).
// Les Statement sont manipulés par référence et en Vec, jamais copiés en
// masse : le coût réel est faible. À reconsidérer si le parsing de gros
// fichiers devient un point chaud mesuré.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Statement {
    Select(SelectStmt),
    Insert(InsertStmt),
    Update(UpdateStmt),
    Delete(DeleteStmt),
    Ddl(crate::ddl::DdlStatement),
}

// ── SELECT ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectStmt {
    pub id: NodeId,
    pub span: Span,
    pub with: Vec<Cte>,
    pub distinct: bool,
    pub columns: Vec<SelectItem>,
    pub from: Vec<TableRef>,
    pub joins: Vec<Join>,
    pub where_clause: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    pub order_by: Vec<OrderByExpr>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cte {
    pub name: Ident,
    pub columns: Vec<Ident>,
    pub query: Box<SelectStmt>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SelectItem {
    Wildcard,
    QualifiedWildcard(Ident),
    Expr { expr: Expr, alias: Option<Ident> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableRef {
    pub name: ObjectName,
    pub alias: Option<Ident>,
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
    pub table: ObjectName,
    pub columns: Vec<Ident>,
    pub source: InsertSource,
    pub on_conflict: Option<OnConflict>,
    pub returning: Vec<SelectItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum InsertSource {
    Values(Vec<Vec<Expr>>),
    Select(Box<SelectStmt>),
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
    pub column: Ident,
    pub value: Expr,
}

// ── UPDATE ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateStmt {
    pub id: NodeId,
    pub span: Span,
    pub table: TableRef,
    pub assignments: Vec<Assignment>,
    pub from: Vec<TableRef>,
    pub where_clause: Option<Expr>,
    pub returning: Vec<SelectItem>,
}

// ── DELETE ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeleteStmt {
    pub id: NodeId,
    pub span: Span,
    pub table: TableRef,
    pub using: Vec<TableRef>,
    pub where_clause: Option<Expr>,
    pub returning: Vec<SelectItem>,
}

// Suppress unused import warning — DataType is part of the public API surface
// and may be needed by consumers of this module.
const _: Option<DataType> = None;

/// Span du statement, quel que soit son type.
///
/// Seuls les nœuds de niveau statement portent un span : `Expr`, `Ident` et
/// `ColumnRef` n'en ont pas. C'est donc la granularité maximale disponible
/// aujourd'hui pour situer un diagnostic, et celle qu'utilise le type-checker.
///
/// Renvoie `None` pour les DDL qui n'ont pas de span (les variants autres que
/// CREATE TABLE / FUNCTION / PROCEDURE / POLICY), et pour un span resté à
/// [`Span::DUMMY`] — un span nul pointerait en 1:1, ce qu'on cherche à éviter.
pub fn stmt_span(stmt: &Statement) -> Option<Span> {
    use crate::ddl::DdlStatement;

    let span = match stmt {
        Statement::Select(s) => s.span,
        Statement::Insert(s) => s.span,
        Statement::Update(s) => s.span,
        Statement::Delete(s) => s.span,
        Statement::Ddl(ddl) => match ddl {
            DdlStatement::CreateTable(c) => c.span,
            DdlStatement::CreateFunction(c) => c.span,
            DdlStatement::CreateProcedure(c) => c.span,
            DdlStatement::CreatePolicy(c) => c.span,
            _ => return None,
        },
    };

    if span == Span::DUMMY {
        None
    } else {
        Some(span)
    }
}
