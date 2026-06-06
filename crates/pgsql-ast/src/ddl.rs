use serde::{Deserialize, Serialize};
use crate::expr::{Expr, ObjectName, Ident};
use crate::types::DataType;
use crate::stmt::SelectStmt;
use crate::node::{NodeId, Span};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DdlStatement {
    CreateTable(CreateTable),
    CreateIndex(CreateIndex),
    CreateFunction(CreateFunction),
    CreateProcedure(CreateProcedure),
    CreateTrigger(CreateTrigger),
    CreatePolicy(CreatePolicy),
    AlterTable(AlterTable),
}

// ── CREATE TABLE ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateTable {
    pub id: NodeId,
    pub span: Span,
    pub name: ObjectName,
    pub if_not_exists: bool,
    pub columns: Vec<ColumnDef>,
    pub constraints: Vec<TableConstraint>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: Ident,
    pub data_type: DataType,
    pub nullable: bool,
    pub default: Option<Expr>,
    pub constraints: Vec<ColumnConstraint>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ColumnConstraint {
    PrimaryKey,
    Unique,
    NotNull,
    Check(Expr),
    References {
        table: ObjectName,
        column: Option<Ident>,
        on_delete: Option<ReferentialAction>,
        on_update: Option<ReferentialAction>,
    },
    Default(Expr),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReferentialAction {
    Cascade,
    SetNull,
    SetDefault,
    Restrict,
    NoAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TableConstraint {
    PrimaryKey { columns: Vec<Ident> },
    Unique { columns: Vec<Ident> },
    ForeignKey {
        columns: Vec<Ident>,
        ref_table: ObjectName,
        ref_columns: Vec<Ident>,
        on_delete: Option<ReferentialAction>,
        on_update: Option<ReferentialAction>,
    },
    Check(Expr),
}

// ── CREATE INDEX ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateIndex {
    pub name: Option<Ident>,
    pub table: ObjectName,
    pub columns: Vec<Ident>,
    pub unique: bool,
    pub if_not_exists: bool,
}

// ── CREATE FUNCTION / PROCEDURE ──────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionParam {
    pub name: Option<Ident>,
    pub data_type: DataType,
    pub mode: ParamMode,
    pub default: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ParamMode {
    In,
    Out,
    InOut,
    Variadic,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FunctionLanguage {
    Sql,
    PlPgSql,
    C,
    Internal,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FunctionBody {
    /// Corps brut non parsé (ou parse échoué avec dégradation gracieuse)
    Raw(String),
    /// Corps LANGUAGE sql parsé comme un SELECT
    SqlQuery(Box<SelectStmt>),
    /// Corps LANGUAGE plpgsql parsé en AST structuré
    Parsed(Box<crate::plpgsql::PlpgsqlBody>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReturnType {
    Scalar(DataType),
    Table(Vec<(Ident, DataType)>),
    SetOf(DataType),
    Trigger,
    Void,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateFunction {
    pub id: NodeId,
    pub span: Span,
    pub name: ObjectName,
    pub or_replace: bool,
    pub params: Vec<FunctionParam>,
    pub return_type: ReturnType,
    pub language: FunctionLanguage,
    pub body: FunctionBody,
    pub security_definer: bool,
    pub volatility: Volatility,
    pub cost: Option<u64>,
    pub rows: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Volatility {
    Volatile,
    Stable,
    Immutable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateProcedure {
    pub id: NodeId,
    pub span: Span,
    pub name: ObjectName,
    pub or_replace: bool,
    pub params: Vec<FunctionParam>,
    pub language: FunctionLanguage,
    pub body: FunctionBody,
}

// ── CREATE TRIGGER ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateTrigger {
    pub name: Ident,
    pub table: ObjectName,
    pub timing: TriggerTiming,
    pub events: Vec<TriggerEvent>,
    pub for_each: TriggerForEach,
    pub when: Option<Expr>,
    pub function: ObjectName,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TriggerTiming {
    Before,
    After,
    InsteadOf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TriggerEvent {
    Insert,
    Update(Vec<Ident>),
    Delete,
    Truncate,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TriggerForEach {
    Row,
    Statement,
}

// ── CREATE POLICY ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatePolicy {
    pub id: NodeId,
    pub span: Span,
    pub name: Ident,
    pub table: ObjectName,
    pub command: PolicyCommand,
    pub permissive: bool,
    pub roles: Vec<Ident>,
    pub using: Option<Expr>,
    pub with_check: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PolicyCommand {
    All,
    Select,
    Insert,
    Update,
    Delete,
}

// ── ALTER TABLE ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlterTable {
    pub name: ObjectName,
    pub operations: Vec<AlterTableOp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AlterTableOp {
    EnableRowLevelSecurity,
    DisableRowLevelSecurity,
    ForceRowLevelSecurity,
    AddColumn(ColumnDef),
    DropColumn { name: Ident, if_exists: bool },
    AlterColumn { name: Ident, action: AlterColumnAction },
    AddConstraint(TableConstraint),
    DropConstraint { name: Ident, if_exists: bool },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AlterColumnAction {
    SetDefault(Expr),
    DropDefault,
    SetNotNull,
    DropNotNull,
    SetDataType(DataType),
}
