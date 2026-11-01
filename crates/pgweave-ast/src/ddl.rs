use crate::expr::{Expr, Ident, ObjectName};
use crate::node::{NodeId, Span};
use crate::stmt::SelectStmt;
use crate::types::DataType;
use serde::{Deserialize, Serialize};

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
    pub temporary: bool,
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
    PrimaryKey {
        columns: Vec<Ident>,
    },
    Unique {
        columns: Vec<Ident>,
    },
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
    /// Corps non analysé : langage externe (`C`, `internal` — le corps est un
    /// symbole), ou corps dont l'analyse a échoué (signalé à part).
    Raw(String),
    /// Corps `LANGUAGE sql` réduit à un seul SELECT.
    SqlQuery(Box<SelectStmt>),
    /// Corps `LANGUAGE sql` de plusieurs statements, ou `BEGIN ATOMIC … END`.
    /// Le résultat de la routine est celui du dernier statement.
    Sql(Vec<crate::stmt::Statement>),
    /// Corps SQL standard `RETURN expr`.
    SqlReturn(Expr),
    /// Corps LANGUAGE plpgsql parsé en AST structuré
    Parsed(Box<crate::plpgsql::PlpgsqlBody>),
}

/// `PARALLEL { UNSAFE | RESTRICTED | SAFE }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Parallel {
    Unsafe,
    Restricted,
    Safe,
}

/// Clause `SET param { TO | = } valeur [, …]` ou `SET param FROM CURRENT`
/// d'une routine : la valeur s'applique pendant l'exécution du corps.
/// Indispensable pour `search_path` (résolution des noms du corps, et règle
/// `SECURITY DEFINER` sans `search_path` fixé).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionSetting {
    pub name: String,
    pub value: SettingValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettingValue {
    /// Valeurs, telles qu'écrites (identifiants sans guillemets, chaînes sans
    /// délimiteurs). `DEFAULT` donne une liste vide.
    Values(Vec<String>),
    FromCurrent,
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
    /// Byte offset in the source file where the body (AS $$) begins.
    /// Set to 0 when unknown (e.g. non-dollar-quoted bodies).
    pub body_start: u32,
    pub name: ObjectName,
    pub or_replace: bool,
    pub params: Vec<FunctionParam>,
    pub return_type: ReturnType,
    pub language: FunctionLanguage,
    pub body: FunctionBody,
    pub security_definer: bool,
    pub volatility: Volatility,
    /// `STRICT` / `RETURNS NULL ON NULL INPUT` : renvoie NULL dès qu'un
    /// argument est NULL. Compte pour l'inférence de nullabilité.
    pub strict: bool,
    pub leakproof: bool,
    pub parallel: Option<Parallel>,
    pub cost: Option<u64>,
    pub rows: Option<u64>,
    pub settings: Vec<FunctionSetting>,
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
    /// Byte offset in the source file where the body (AS $$) begins.
    pub body_start: u32,
    pub name: ObjectName,
    pub or_replace: bool,
    pub params: Vec<FunctionParam>,
    pub language: FunctionLanguage,
    pub body: FunctionBody,
    pub security_definer: bool,
    pub settings: Vec<FunctionSetting>,
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
    DropColumn {
        name: Ident,
        if_exists: bool,
    },
    AlterColumn {
        name: Ident,
        action: AlterColumnAction,
    },
    AddConstraint(TableConstraint),
    DropConstraint {
        name: Ident,
        if_exists: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AlterColumnAction {
    SetDefault(Expr),
    DropDefault,
    SetNotNull,
    DropNotNull,
    SetDataType(DataType),
}
