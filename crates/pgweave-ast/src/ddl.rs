use crate::expr::{Expr, Ident, ObjectName};
use crate::node::{NodeId, Span};
use crate::stmt::SelectStmt;
use crate::types::DataType;
use serde::{Deserialize, Serialize};

// Même arbitrage que pour `Statement` (voir stmt.rs) : boxer les variants
// casserait le filtrage par motif dans tous les crates consommateurs, pour un
// gain négligeable sur des nœuds manipulés par référence.
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
    /// `UNLOGGED` : non journalisée (perdue après un crash).
    pub unlogged: bool,
    /// `ON COMMIT …`, tables temporaires seulement.
    pub on_commit: Option<OnCommit>,
    pub columns: Vec<ColumnDef>,
    pub constraints: Vec<TableConstraint>,
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

/// Colonne calculée. Elle n'accepte aucune valeur en `INSERT`/`UPDATE`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeneratedColumn {
    pub expr: Expr,
    /// `STORED` : calculée à l'écriture. `VIRTUAL` (défaut de PostgreSQL 18) :
    /// calculée à la lecture.
    pub stored: bool,
}

/// Colonne identité : valeur tirée d'une séquence implicite. `Always`
/// refuse une valeur explicite en `INSERT` (sauf `OVERRIDING SYSTEM VALUE`).
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

/// Contrainte de table. `name` est le nom donné par `CONSTRAINT nom`, absent
/// quand PostgreSQL en génère un.
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
    /// `EXCLUDE [USING méthode] (élément WITH opérateur, …) [WHERE (…)]`.
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
    /// Opérateur de comparaison (`=`, `&&`, …).
    pub operator: String,
}

// ── CREATE INDEX ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateIndex {
    pub name: Option<Ident>,
    pub table: ObjectName,
    /// Éléments indexés, dans l'ordre : colonnes ou expressions.
    pub elements: Vec<IndexElement>,
    pub unique: bool,
    pub if_not_exists: bool,
    pub concurrently: bool,
    /// Méthode d'accès (`btree`, `gin`, `gist`…), absente = btree.
    pub using: Option<String>,
    /// Index partiel.
    pub where_clause: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IndexElement {
    Column(Ident),
    Expr(Expr),
}

impl CreateIndex {
    /// Colonnes indexées directement (hors expressions).
    pub fn columns(&self) -> impl Iterator<Item = &Ident> {
        self.elements.iter().filter_map(|e| match e {
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
    pub or_replace: bool,
    pub table: ObjectName,
    pub timing: TriggerTiming,
    pub events: Vec<TriggerEvent>,
    pub for_each: TriggerForEach,
    /// `REFERENCING { OLD | NEW } TABLE AS nom` : tables de transition, visibles
    /// dans le corps d'un trigger niveau statement.
    pub referencing: Vec<TransitionTable>,
    pub when: Option<Expr>,
    pub function: ObjectName,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TransitionTable {
    /// `NEW TABLE` (lignes insérées ou nouvelles valeurs) ou `OLD TABLE`.
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
    RenameColumn {
        from: Ident,
        to: Ident,
    },
    /// `RENAME TO` : nouveau nom, dans le même schéma.
    RenameTo(Ident),
    /// `SET SCHEMA` : déplacement, pas suppression puis création.
    SetSchema(Ident),
    ValidateConstraint(Ident),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum AlterColumnAction {
    SetDefault(Expr),
    DropDefault,
    SetNotNull,
    DropNotNull,
    SetDataType(DataType),
}

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateType {
    pub name: ObjectName,
    pub kind: TypeDefinition,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TypeDefinition {
    /// `AS ENUM ('a', 'b')` : valeurs dans l'ordre de déclaration (qui est
    /// l'ordre de tri).
    Enum(Vec<String>),
    /// `AS (champ type, …)`.
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
        /// Position relative à une valeur existante ; absente = à la fin.
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
    /// `CHECK (expr)` où `VALUE` désigne la valeur testée.
    pub checks: Vec<DomainCheck>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainCheck {
    pub name: Option<Ident>,
    pub expr: Expr,
}

// ── Extensions et schémas ─────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateExtension {
    pub name: Ident,
    pub if_not_exists: bool,
    /// Schéma où ranger les objets de l'extension (PLAN.md, D11).
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
    /// `UPDATE [TO version]` : sans version, la version par défaut installée.
    Update(Option<String>),
    SetSchema(Ident),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSchema {
    pub name: Ident,
    pub if_not_exists: bool,
}

// ── Vues et séquences ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateView {
    pub name: ObjectName,
    pub or_replace: bool,
    pub materialized: bool,
    pub temporary: bool,
    /// Noms de colonnes explicites `CREATE VIEW v (a, b) AS …`.
    pub columns: Vec<Ident>,
    pub query: SelectStmt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateSequence {
    pub name: ObjectName,
    pub if_not_exists: bool,
    pub temporary: bool,
    /// `AS type` : `bigint` par défaut.
    pub data_type: Option<DataType>,
    pub increment: Option<i64>,
    pub min_value: Option<i64>,
    pub max_value: Option<i64>,
    pub start: Option<i64>,
    pub cache: Option<i64>,
    pub cycle: bool,
    /// `OWNED BY table.colonne`.
    pub owned_by: Option<ObjectName>,
}

// ── DROP ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DropObject {
    pub kind: ObjectKind,
    pub names: Vec<ObjectName>,
    pub if_exists: bool,
    pub cascade: bool,
    /// Table portant l'objet : `DROP TRIGGER t ON table`, `DROP POLICY p ON
    /// table`.
    pub on_table: Option<ObjectName>,
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
