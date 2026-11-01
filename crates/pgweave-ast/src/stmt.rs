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
    Utility(crate::utility::Utility),
}

// ── SELECT ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectStmt {
    pub id: NodeId,
    pub span: Span,
    pub with: Vec<Cte>,
    /// `WITH RECURSIVE` : les CTE peuvent se référencer elles-mêmes.
    pub with_recursive: bool,
    pub distinct: bool,
    /// `DISTINCT ON (expr, …)` : une ligne par valeur de ces expressions
    /// (implique `distinct`).
    pub distinct_on: Vec<Expr>,
    pub columns: Vec<SelectItem>,
    pub from: Vec<TableRef>,
    pub joins: Vec<Join>,
    pub where_clause: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
    /// Clause `WINDOW nom AS (…)`, dans l'ordre d'écriture.
    pub windows: Vec<NamedWindow>,
    pub order_by: Vec<OrderByExpr>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
    /// `UNION` / `INTERSECT` / `EXCEPT` qui suivent ce SELECT, appliqués de
    /// gauche à droite ; chaque opérande peut porter ses propres opérations
    /// (`a UNION b INTERSECT c` = `a UNION (b INTERSECT c)`). Les colonnes
    /// du résultat sont nommées par ce premier SELECT ; `order_by`, `limit` et
    /// `offset` s'appliquent alors à l'ensemble.
    pub set_ops: Vec<SetOperation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SetOperation {
    pub op: SetOperator,
    /// `ALL` : garde les doublons.
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
    pub query: Box<SelectStmt>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SelectItem {
    Wildcard,
    QualifiedWildcard(Ident),
    Expr { expr: Expr, alias: Option<Ident> },
}

/// Table modifiée par `UPDATE` ou `DELETE` : toujours une table nommée.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetTable {
    pub name: ObjectName,
    pub alias: Option<Ident>,
}

/// Élément de `FROM` ou de jointure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TableRef {
    pub source: TableSource,
    pub alias: Option<Ident>,
    /// `AS alias (c1, c2)` : renomme les colonnes de la source, dans l'ordre.
    pub column_aliases: Vec<Ident>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TableSource {
    /// Table, vue ou CTE nommée.
    Table(ObjectName),
    /// `(SELECT …)`, éventuellement `LATERAL` (voit les sources précédentes).
    Subquery {
        lateral: bool,
        query: Box<SelectStmt>,
    },
    /// Fonction renvoyant des lignes : `generate_series(1, 3)`,
    /// `unnest(arr)`, fonction `RETURNS TABLE` du projet.
    Function {
        lateral: bool,
        name: ObjectName,
        args: Vec<Expr>,
        with_ordinality: bool,
    },
}

impl TableRef {
    /// Table nommée, si la source en est une.
    pub fn table_name(&self) -> Option<&ObjectName> {
        match &self.source {
            TableSource::Table(n) => Some(n),
            _ => None,
        }
    }

    /// Nom sous lequel la source est visible dans la requête : son alias,
    /// sinon le dernier segment de son nom (table ou fonction).
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
    /// `CROSS JOIN` (ou virgule) : produit cartésien, sans condition.
    None,
}

/// Noms donnés aux lignes avant (`OLD`) et après (`NEW`) modification dans
/// le `RETURNING` d'un statement de modification (PostgreSQL 18). Absents,
/// ce sont `old` et `new`.
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

/// `WINDOW nom AS (spécification)`.
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
    /// `WITH` en tête du statement : CTE visibles de tout le statement.
    pub with: Vec<Cte>,
    pub with_recursive: bool,
    pub table: ObjectName,
    /// `INSERT INTO t AS alias` : nom de la table cible dans `ON CONFLICT` et
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
    /// `DEFAULT VALUES` : une ligne, chaque colonne à sa valeur par défaut.
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
    /// Colonnes affectées : une seule (`a = v`), ou plusieurs pour la forme
    /// multi-colonnes `(a, b) = (v1, v2)` / `(a, b) = (SELECT …)`.
    pub columns: Vec<Ident>,
    pub value: Expr,
}

// ── UPDATE ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UpdateStmt {
    pub id: NodeId,
    pub span: Span,
    /// `WITH` en tête du statement : CTE visibles de tout le statement.
    pub with: Vec<Cte>,
    pub with_recursive: bool,
    pub table: TargetTable,
    pub assignments: Vec<Assignment>,
    pub from: Vec<TableRef>,
    /// Jointures écrites dans `FROM`.
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
    /// `WITH` en tête du statement : CTE visibles de tout le statement.
    pub with: Vec<Cte>,
    pub with_recursive: bool,
    pub table: TargetTable,
    pub using: Vec<TableRef>,
    /// Jointures écrites dans `USING`.
    pub joins: Vec<Join>,
    pub where_clause: Option<Expr>,
    pub returning: Vec<SelectItem>,
    /// `RETURNING WITH (OLD AS o, NEW AS n)` (PostgreSQL 18).
    pub returning_aliases: ReturningAliases,
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
        Statement::Utility(u) => u.span,
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
