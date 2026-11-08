use crate::expr::{Expr, Ident, ObjectName};
use crate::node::{NodeId, Span};
use crate::stmt::SelectStmt;
use crate::types::DataType;
use serde::{Deserialize, Serialize};

// Same trade-off as for `Statement` (see stmt.rs): boxing the variants
// would break pattern matching in every consumer crate, for a negligible
// gain on nodes handled by reference.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DdlStatement {
    CreateTable(CreateTable),
    CreateIndex(CreateIndex),
    CreateFunction(CreateFunction),
    CreateProcedure(CreateProcedure),
    CreateTrigger(CreateTrigger),
    CreatePolicy(CreatePolicy),
    AlterTable(AlterTable),
    CreateType(CreateType),
    AlterType(AlterType),
    CreateDomain(CreateDomain),
    CreateExtension(CreateExtension),
    AlterExtension(AlterExtension),
    CreateSchema(CreateSchema),
    CreateView(CreateView),
    CreateSequence(CreateSequence),
    Drop(DropObject),
}

// ── CREATE TABLE ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateTable {
    pub id: NodeId,
    pub span: Span,
    pub name: ObjectName,
    pub if_not_exists: bool,
    pub temporary: bool,
    /// `UNLOGGED`: not WAL-logged (lost after a crash).
    pub unlogged: bool,
    /// `ON COMMIT …`, temporary tables only.
    pub on_commit: Option<OnCommit>,
    pub columns: Vec<ColumnDef>,
    pub constraints: Vec<TableConstraint>,
    /// `LIKE` elements: columns copied from another table.
    pub likes: Vec<TableLike>,
    /// `CREATE TABLE … AS query`: the columns are those of the query
    /// (`columns` then only gives their names, if not empty).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_query: Option<Box<crate::stmt::SelectStmt>>,
}

/// `LIKE source [{INCLUDING | EXCLUDING} option …]` among the elements of a
/// `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableLike {
    pub source: ObjectName,
    /// Number of columns declared before this element: the copied columns
    /// are inserted at this position, as in PostgreSQL.
    pub position: usize,
    /// Options in writing order (the last one wins).
    pub options: Vec<LikeOption>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LikeOption {
    pub including: bool,
    pub feature: LikeFeature,
}

/// What `LIKE` copies in addition to names, types and `NOT NULL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LikeFeature {
    Comments,
    Compression,
    Constraints,
    Defaults,
    Generated,
    Identity,
    Indexes,
    Statistics,
    Storage,
    All,
}

impl LikeFeature {
    pub fn keyword(self) -> &'static str {
        match self {
            LikeFeature::Comments => "COMMENTS",
            LikeFeature::Compression => "COMPRESSION",
            LikeFeature::Constraints => "CONSTRAINTS",
            LikeFeature::Defaults => "DEFAULTS",
            LikeFeature::Generated => "GENERATED",
            LikeFeature::Identity => "IDENTITY",
            LikeFeature::Indexes => "INDEXES",
            LikeFeature::Statistics => "STATISTICS",
            LikeFeature::Storage => "STORAGE",
            LikeFeature::All => "ALL",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OnCommit {
    PreserveRows,
    DeleteRows,
    Drop,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnDef {
    pub name: Ident,
    pub data_type: DataType,
    pub nullable: bool,
    pub default: Option<Expr>,
    /// `GENERATED ALWAYS AS (expr) [STORED | VIRTUAL]`.
    pub generated: Option<GeneratedColumn>,
    /// `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY`.
    pub identity: Option<Identity>,
    pub constraints: Vec<ColumnConstraint>,
}

/// Generated column. It accepts no value in `INSERT`/`UPDATE`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeneratedColumn {
    pub expr: Expr,
    /// `STORED`: computed on write. `VIRTUAL` (PostgreSQL 18 default):
    /// computed on read.
    pub stored: bool,
}

/// Identity column: value drawn from an implicit sequence. `Always`
/// rejects an explicit value in `INSERT` (unless `OVERRIDING SYSTEM VALUE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Identity {
    Always,
    ByDefault,
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

/// Table constraint. `name` is the name given by `CONSTRAINT name`, absent
/// when PostgreSQL generates one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TableConstraint {
    PrimaryKey {
        name: Option<Ident>,
        columns: Vec<Ident>,
    },
    Unique {
        name: Option<Ident>,
        columns: Vec<Ident>,
    },
    ForeignKey {
        name: Option<Ident>,
        columns: Vec<Ident>,
        ref_table: ObjectName,
        ref_columns: Vec<Ident>,
        on_delete: Option<ReferentialAction>,
        on_update: Option<ReferentialAction>,
    },
    Check {
        name: Option<Ident>,
        expr: Expr,
    },
    /// `EXCLUDE [USING method] (element WITH operator, …) [WHERE (…)]`.
    Exclude {
        name: Option<Ident>,
        using: Option<String>,
        elements: Vec<ExclusionElement>,
        where_clause: Option<Expr>,
    },
}

impl TableConstraint {
    pub fn name(&self) -> Option<&Ident> {
        match self {
            TableConstraint::PrimaryKey { name, .. }
            | TableConstraint::Unique { name, .. }
            | TableConstraint::ForeignKey { name, .. }
            | TableConstraint::Check { name, .. }
            | TableConstraint::Exclude { name, .. } => name.as_ref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExclusionElement {
    pub element: IndexElement,
    /// Comparison operator (`=`, `&&`, …).
    pub operator: String,
}

// ── CREATE INDEX ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateIndex {
    pub span: Span,
    pub name: Option<Ident>,
    pub table: ObjectName,
    /// Indexed elements, in order: columns or expressions, with their
    /// operator class and ordering.
    pub elements: Vec<IndexColumn>,
    /// `INCLUDE (…)`: columns carried without being indexed.
    pub include: Vec<Ident>,
    /// `NULLS NOT DISTINCT`: a unique index rejects two NULLs.
    pub nulls_not_distinct: bool,
    pub unique: bool,
    pub if_not_exists: bool,
    pub concurrently: bool,
    /// Access method (`btree`, `gin`, `gist`…), absent = btree.
    pub using: Option<String>,
    /// Partial index.
    pub where_clause: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IndexElement {
    Column(Ident),
    Expr(Expr),
}

/// An index element and its options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexColumn {
    pub element: IndexElement,
    /// Operator class (`gin_trgm_ops`); absent = the type's default one for
    /// the method.
    pub opclass: Option<ObjectName>,
    /// `DESC`; otherwise ascending.
    pub descending: bool,
    /// `NULLS FIRST` (`Some(true)`) or `NULLS LAST` (`Some(false)`) written.
    pub nulls_first: Option<bool>,
}

impl IndexColumn {
    pub fn plain(element: IndexElement) -> Self {
        IndexColumn {
            element,
            opclass: None,
            descending: false,
            nulls_first: None,
        }
    }
}

impl CreateIndex {
    /// Directly indexed columns (expressions excluded).
    pub fn columns(&self) -> impl Iterator<Item = &Ident> {
        self.elements.iter().filter_map(|e| match &e.element {
            IndexElement::Column(c) => Some(c),
            IndexElement::Expr(_) => None,
        })
    }
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
    /// Unanalyzed body: external language (`C`, `internal` — the body is a
    /// symbol), or body whose analysis failed (reported separately).
    Raw(String),
    /// `LANGUAGE sql` body reduced to a single SELECT.
    SqlQuery(Box<SelectStmt>),
    /// `LANGUAGE sql` body with several statements, or `BEGIN ATOMIC … END`.
    /// The routine's result is that of the last statement.
    Sql(Vec<crate::stmt::Statement>),
    /// Standard SQL body `RETURN expr`.
    SqlReturn(Expr),
    /// LANGUAGE plpgsql body parsed into a structured AST
    Parsed(Box<crate::plpgsql::PlpgsqlBody>),
}

/// `PARALLEL { UNSAFE | RESTRICTED | SAFE }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Parallel {
    Unsafe,
    Restricted,
    Safe,
}

/// `SET param { TO | = } value [, …]` or `SET param FROM CURRENT` clause
/// of a routine: the value applies while the body executes.
/// Essential for `search_path` (name resolution in the body, and the
/// `SECURITY DEFINER` without a fixed `search_path` rule).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionSetting {
    pub name: String,
    pub value: SettingValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SettingValue {
    /// Values, as written (identifiers without quotes, strings without
    /// delimiters). `DEFAULT` gives an empty list.
    Values(Vec<String>),
    FromCurrent,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReturnType {
    /// No `RETURNS` clause: the result is defined by the `OUT`/`INOUT`
    /// parameters — the type of the single parameter, or `record` if there
    /// are several. A single row (not `SETOF`).
    OutParams,
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
    /// `STRICT` / `RETURNS NULL ON NULL INPUT`: returns NULL as soon as an
    /// argument is NULL. Counts for nullability inference.
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
    pub or_replace: bool,
    pub table: ObjectName,
    pub timing: TriggerTiming,
    pub events: Vec<TriggerEvent>,
    pub for_each: TriggerForEach,
    /// `REFERENCING { OLD | NEW } TABLE AS name`: transition tables, visible
    /// in the body of a statement-level trigger.
    pub referencing: Vec<TransitionTable>,
    pub when: Option<Expr>,
    pub function: ObjectName,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransitionTable {
    /// `NEW TABLE` (inserted rows or new values) or `OLD TABLE`.
    pub new: bool,
    pub name: Ident,
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
    pub if_exists: bool,
    /// `ONLY`: the table alone, without its child tables (inheritance,
    /// partitions).
    pub only: bool,
    pub operations: Vec<AlterTableOp>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AlterTableOp {
    EnableRowLevelSecurity,
    DisableRowLevelSecurity,
    ForceRowLevelSecurity,
    NoForceRowLevelSecurity,
    AddColumn {
        column: ColumnDef,
        if_not_exists: bool,
    },
    DropColumn {
        name: Ident,
        if_exists: bool,
        /// `CASCADE`: also drops the objects that depend on it.
        cascade: bool,
    },
    AlterColumn {
        name: Ident,
        action: AlterColumnAction,
    },
    AddConstraint {
        constraint: TableConstraint,
        /// `NOT VALID`: existing rows are not checked (see
        /// `ValidateConstraint`).
        not_valid: bool,
    },
    DropConstraint {
        name: Ident,
        if_exists: bool,
        cascade: bool,
    },
    RenameColumn {
        from: Ident,
        to: Ident,
    },
    /// `RENAME TO`: new name, in the same schema.
    RenameTo(Ident),
    /// `SET SCHEMA`: a move, not a drop followed by a create.
    SetSchema(Ident),
    ValidateConstraint(Ident),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AlterColumnAction {
    SetDefault(Expr),
    DropDefault,
    SetNotNull,
    DropNotNull,
    /// `[SET DATA] TYPE t [USING expr]`: `using` converts the existing
    /// values.
    SetDataType {
        data_type: DataType,
        using: Option<Expr>,
    },
}

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateType {
    pub name: ObjectName,
    pub kind: TypeDefinition,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TypeDefinition {
    /// `AS ENUM ('a', 'b')`: values in declaration order (which is the
    /// sort order).
    Enum(Vec<String>),
    /// `AS (field type, …)`.
    Composite(Vec<(Ident, DataType)>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlterType {
    pub name: ObjectName,
    pub operation: AlterTypeOp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AlterTypeOp {
    AddValue {
        value: String,
        if_not_exists: bool,
        /// Position relative to an existing value; absent = at the end.
        position: Option<EnumValuePosition>,
    },
    RenameValue {
        from: String,
        to: String,
    },
    RenameTo(Ident),
    SetSchema(Ident),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EnumValuePosition {
    Before(String),
    After(String),
}

// ── CREATE DOMAIN ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateDomain {
    pub name: ObjectName,
    pub data_type: DataType,
    pub not_null: bool,
    pub default: Option<Expr>,
    /// `CHECK (expr)` where `VALUE` denotes the tested value.
    pub checks: Vec<DomainCheck>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainCheck {
    pub name: Option<Ident>,
    pub expr: Expr,
}

// ── Extensions and schemas ────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateExtension {
    pub name: Ident,
    pub if_not_exists: bool,
    /// Schema where the extension's objects are placed (PLAN.md, D11).
    pub schema: Option<Ident>,
    pub version: Option<String>,
    pub cascade: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlterExtension {
    pub name: Ident,
    pub operation: AlterExtensionOp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AlterExtensionOp {
    /// `UPDATE [TO version]`: without a version, the default version is installed.
    Update(Option<String>),
    SetSchema(Ident),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSchema {
    pub name: Ident,
    pub if_not_exists: bool,
    /// `AUTHORIZATION role`: owner of the schema.
    pub authorization: Option<Ident>,
}

// ── Views and sequences ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateView {
    pub name: ObjectName,
    pub or_replace: bool,
    pub materialized: bool,
    pub temporary: bool,
    /// Explicit column names `CREATE VIEW v (a, b) AS …`.
    pub columns: Vec<Ident>,
    pub query: SelectStmt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSequence {
    pub name: ObjectName,
    pub if_not_exists: bool,
    pub temporary: bool,
    /// `AS type`: `bigint` by default.
    pub data_type: Option<DataType>,
    pub increment: Option<i64>,
    pub min_value: Option<i64>,
    pub max_value: Option<i64>,
    pub start: Option<i64>,
    pub cache: Option<i64>,
    pub cycle: bool,
    /// `OWNED BY table.column`.
    pub owned_by: Option<ObjectName>,
}

// ── DROP ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DropObject {
    pub kind: ObjectKind,
    pub names: Vec<ObjectName>,
    pub if_exists: bool,
    pub cascade: bool,
    /// Table carrying the object: `DROP TRIGGER t ON table`, `DROP POLICY p ON
    /// table`.
    pub on_table: Option<ObjectName>,
    /// Functions and procedures: input argument types of each name
    /// (`DROP FUNCTION f(int)`), `None` without parentheses. Parallel to `names`
    /// for these object kinds, empty otherwise.
    pub signatures: Vec<Option<Vec<DataType>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectKind {
    Table,
    View,
    MaterializedView,
    Index,
    Sequence,
    Type,
    Domain,
    Function,
    Procedure,
    Trigger,
    Policy,
    Schema,
    Extension,
}
